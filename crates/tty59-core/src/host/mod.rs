pub mod conformance;
pub mod content_search;
pub mod local;
pub mod remote;
pub mod server;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread::ThreadId;

pub use crate::core::shells::ShellInventory;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct HostId(pub u64);

impl HostId {
    pub const LOCAL: HostId = HostId(0);

    pub fn from_connection_key(key: &str) -> HostId {
        let h = fnv1a64(key.as_bytes());
        HostId(if h == 0 { 1 } else { h })
    }

    pub fn is_local(self) -> bool {
        self == HostId::LOCAL
    }
}

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

static UI_THREAD: OnceLock<ThreadId> = OnceLock::new();

pub fn register_ui_thread() {
    let _ = UI_THREAD.set(std::thread::current().id());
}

pub fn is_ui_thread() -> bool {
    UI_THREAD
        .get()
        .is_some_and(|t| *t == std::thread::current().id())
}

#[inline]
pub fn guard_off_ui() {
    debug_assert!(
        !is_ui_thread(),
        "Host call on the UI thread — route it through ui::host_ops::HostOps"
    );
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub ignored: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub is_dir: bool,
    pub is_symlink: bool,
    pub len: u64,
    pub mtime: Option<MTime>,
    pub readonly: bool,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct MTime {
    pub secs: i64,
    pub nanos: u32,
}

impl MTime {
    pub fn from_system_time(t: std::time::SystemTime) -> MTime {
        match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => MTime {
                secs: d.as_secs() as i64,
                nanos: d.subsec_nanos(),
            },
            Err(e) => {
                let d = e.duration();
                let (secs, nanos) = if d.subsec_nanos() == 0 {
                    (-(d.as_secs() as i64), 0)
                } else {
                    (-(d.as_secs() as i64) - 1, 1_000_000_000 - d.subsec_nanos())
                };
                MTime { secs, nanos }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Output {
    pub status: Option<i32>,
    #[serde(with = "b64")]
    pub stdout: Vec<u8>,
    #[serde(with = "b64")]
    pub stderr: Vec<u8>,
}

pub(crate) mod b64 {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stdout_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }

    pub fn stderr_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub ignored: bool,
}

/// What [`Host::search_content`] looks for.
///
/// Every flag defaults to off, so a peer that grows a field later still reads
/// an older client's query the way that client meant it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ContentQuery {
    /// The text to find — or, with `regex`, the expression.
    pub pattern: String,
    #[serde(default)]
    pub case_sensitive: bool,
    /// Only where the match starts and ends on a word boundary.
    #[serde(default)]
    pub whole_word: bool,
    /// `pattern` is a regular expression (Rust `regex` syntax) rather than
    /// literal text.
    #[serde(default)]
    pub regex: bool,
    /// Walk dot-directories and gitignored paths too, as the name search's
    /// flag of the same name does.
    #[serde(default)]
    pub show_hidden: bool,
}

/// How far one [`Host::search_content`] may go before it stops and says it
/// stopped. Every cap reached is reported as `truncated`, never as silence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ContentLimits {
    /// Matching lines across every file.
    pub max_hits: u64,
    /// Matching lines taken from one file; the rest of that file is skipped.
    pub max_hits_per_file: u64,
    /// Files opened and read. The walk stops at this many.
    pub max_files: u64,
    /// A file longer than this is not read at all — a minified bundle or a
    /// data dump is not source anyone is searching.
    pub max_file_bytes: u64,
    /// Wall-clock budget for the whole search, so a huge tree answers with
    /// what it found instead of running into the caller's deadline.
    pub max_millis: u64,
}

impl Default for ContentLimits {
    fn default() -> Self {
        ContentLimits {
            max_hits: 2000,
            max_hits_per_file: 100,
            max_files: 20_000,
            max_file_bytes: 1024 * 1024,
            max_millis: 10_000,
        }
    }
}

/// One matching line.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContentHit {
    pub path: PathBuf,
    /// 1-based.
    pub line: u32,
    /// 1-based, in characters, of the first match on the line in the file —
    /// what an editor's cursor wants, independent of how `text` was cut.
    pub column: u32,
    /// The line as shown: leading whitespace dropped, and a line too long to
    /// show cut down to a window around its first match, with `…` marking
    /// each cut.
    pub text: String,
    /// Byte ranges of each match within `text`, in order, never overlapping,
    /// always on character boundaries.
    pub ranges: Vec<ContentRange>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContentRange {
    pub start: u32,
    pub end: u32,
}

/// What a [`Host::search_content`] found, in walk order: files sorted by path
/// within each directory, lines in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContentResults {
    pub hits: Vec<ContentHit>,
    /// A limit in [`ContentLimits`] cut the search short, so there may be
    /// more matches than `hits` holds.
    pub truncated: bool,
    /// How many files were actually read.
    #[serde(default)]
    pub files_searched: u64,
}

pub struct WatchSub {
    rx: smol::channel::Receiver<Vec<PathBuf>>,
    inner: Box<dyn WatchHandle>,
}

impl WatchSub {
    pub fn new(rx: smol::channel::Receiver<Vec<PathBuf>>, inner: Box<dyn WatchHandle>) -> WatchSub {
        WatchSub { rx, inner }
    }

    pub fn events(&self) -> &smol::channel::Receiver<Vec<PathBuf>> {
        &self.rx
    }

    pub fn set_dirs(&self, dirs: &[PathBuf]) -> io::Result<()> {
        self.inner.set_dirs(dirs)
    }
}

pub trait WatchHandle: Send + Sync {
    fn set_dirs(&self, dirs: &[PathBuf]) -> io::Result<()>;
}

pub trait Host: Send + Sync + 'static {
    fn id(&self) -> HostId;

    /// How far away this host is: the round trip to it, measured now.
    ///
    /// `None` from a host with no link to measure — the local one, whose
    /// "peer" is this process's own daemon over a Unix socket — and from a
    /// remote one whose link is down or has not answered a ping yet.
    fn link_rtt(&self) -> Option<std::time::Duration> {
        None
    }

    fn separator(&self) -> char;

    fn join(&self, dir: &Path, name: &str) -> PathBuf {
        default_join(dir, name, self.separator())
    }

    fn is_absolute(&self, p: &Path) -> bool;

    fn read_dir(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<Entry>>;

    fn stat(&self, p: &Path) -> io::Result<Meta>;

    fn exists(&self, p: &Path) -> bool {
        self.stat(p).is_ok()
    }

    fn read_file(&self, p: &Path, max_bytes: u64) -> io::Result<Vec<u8>>;

    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf>;

    fn search(
        &self,
        roots: &[PathBuf],
        query: &str,
        limit: usize,
        max_dirs: usize,
        show_hidden: bool,
    ) -> io::Result<Vec<SearchHit>>;

    /// Search file *contents* under `roots` — the right panel's Search tab.
    ///
    /// Walks the way [`Host::search`] does: `.gitignore` honoured (with or
    /// without a repository), dot-directories skipped, both unless
    /// `query.show_hidden`. Binary files and files over
    /// `limits.max_file_bytes` are skipped. A pattern that does not compile is
    /// [`io::ErrorKind::InvalidInput`]; a host that cannot search contents at
    /// all is [`io::ErrorKind::Unsupported`], which callers must tell apart
    /// from "no matches".
    fn search_content(
        &self,
        roots: &[PathBuf],
        query: &ContentQuery,
        limits: &ContentLimits,
    ) -> io::Result<ContentResults>;

    fn write_file(&self, p: &Path, bytes: &[u8]) -> io::Result<Meta>;

    fn create_file_new(&self, p: &Path) -> io::Result<()>;

    fn create_dir(&self, p: &Path, recursive: bool) -> io::Result<()>;

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    fn remove(&self, p: &Path, recursive: bool) -> io::Result<()>;

    fn repo_root(&self, p: &Path) -> io::Result<Option<PathBuf>>;

    fn git(&self, cwd: &Path, args: &[&str]) -> io::Result<Output>;

    /// The repository `cwd` is in, answered without running `git` — or
    /// `None` when this host cannot, and [`crate::core::git::probe_repo`]
    /// asks `git` instead.
    ///
    /// Only this machine answers. A remote host could read the same files on
    /// its side, but asking it to would be a request its server has never
    /// heard of, and one of those takes the whole link down; the `git` it
    /// runs over there has no Xcode shim to trip on anyway.
    fn repo_head(&self, _cwd: &Path) -> Option<Option<crate::core::git::head::RepoHead>> {
        None
    }

    /// `git`, but with an explicit ceiling on how long to wait for the far side.
    ///
    /// Network verbs (`fetch`/`pull`/`push`) run for as long as the network
    /// takes, which is minutes, not the seconds an interactive query is allowed.
    ///
    /// `LocalHost` deliberately does not override this: `Command::output()`
    /// blocks until the child exits and has no timeout of its own, so waiting
    /// "until the deadline" and waiting "until git is done" are the same wait —
    /// forwarding to `git` is the honest implementation, not a stub. `RemoteHost`
    /// does override it, because its per-request deadline is sized for
    /// interactive queries and a push walks straight into it.
    ///
    /// The reply is still the plain `Git` request on the wire, so a server that
    /// predates this method serves it unchanged.
    fn git_with_deadline(
        &self,
        cwd: &Path,
        args: &[&str],
        deadline: std::time::Duration,
    ) -> io::Result<Output> {
        let _ = deadline;
        self.git(cwd, args)
    }

    fn git_lines(
        &self,
        cwd: &Path,
        args: &[&str],
        on_line: &mut dyn FnMut(&str),
    ) -> io::Result<Option<i32>> {
        let out = self.git(cwd, args)?;
        let mut split = crate::core::git::LineSplitter::default();
        split.push(&out.stdout, &mut *on_line);
        split.finish(&mut *on_line);
        Ok(out.status)
    }

    fn shells(&self) -> io::Result<ShellInventory>;

    /// Past coding-agent sessions on this host, most recent first.
    /// `known_dirs` are directories open on it (see
    /// [`crate::core::agent_history::scan`]). A host that cannot look has
    /// none to offer.
    fn agent_sessions(
        &self,
        _known_dirs: &[PathBuf],
    ) -> io::Result<Vec<crate::core::agent_history::PastSession>> {
        Ok(Vec::new())
    }

    fn watch(&self, dirs: &[PathBuf]) -> io::Result<WatchSub>;

    fn is_connected(&self) -> bool {
        true
    }

    /// What is running inside one of this host's panes, and what it is
    /// listening on — or `None` where this host cannot say.
    ///
    /// `None` is not "nothing is running": it is the answer from a host whose
    /// panes are somebody else's to describe. The local host gives it, because
    /// its panes belong to the daemon the caller asks directly; so does a peer
    /// too old to know the request. Callers must keep the two apart — an empty
    /// list means the pane really is serving nothing, and drawing "no ports"
    /// over "we could not ask" is how a remote pane came to look idle while a
    /// dev server was up in it.
    fn pane_procs(&self, _pane_id: u64) -> Option<crate::daemon::protocol::PaneProcs> {
        None
    }
}

/// Did this watch event mean something *changed*, or only that something was
/// read?
///
/// The distinction does not exist on macOS, and on Linux it decides whether a
/// watch can feed itself. `notify`'s inotify backend asks for `IN_OPEN`, so
/// every `open(2)` under a watched directory arrives as an event — and the one
/// thing every consumer of a watch does with an event is go and read the
/// directory it came from. Reading `.git/index` to answer "did the repository
/// change" is itself an event saying the repository may have changed, so the
/// answer schedules the next question, at whatever rate the debounce allows,
/// forever. Measured at ~2.6 `git status -uall` per second on an idle Source
/// Control panel before this filter existed (issue #523).
///
/// `IN_CLOSE_WRITE` also arrives as `Access`, and that one is a real write
/// finishing, so it stays. Everything else under `Access` is a reader.
pub fn is_content_change(kind: &notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode};

    !matches!(
        kind,
        notify::EventKind::Access(
            AccessKind::Open(_) | AccessKind::Read | AccessKind::Close(AccessMode::Read),
        )
    )
}

pub fn default_join(dir: &Path, name: &str, sep: char) -> PathBuf {
    let mut s = dir.to_string_lossy().into_owned();
    if !s.is_empty() && !s.ends_with(sep) && !s.ends_with('/') {
        s.push(sep);
    }
    s.push_str(name);
    PathBuf::from(s)
}

pub type SharedHost = Arc<dyn Host>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_matches_the_published_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn zero_is_reserved_for_local() {
        assert!(HostId::LOCAL.is_local());
        for i in 0..2000u32 {
            let key = format!("ssh-direct:me@box{i}:22");
            assert!(!HostId::from_connection_key(&key).is_local());
        }
        assert!(!HostId::from_connection_key("").is_local());
    }

    #[test]
    fn connection_keys_map_to_stable_ids() {
        let a = HostId::from_connection_key("ssh-direct:me@box:22");
        assert_eq!(a, HostId::from_connection_key("ssh-direct:me@box:22"));
        assert_ne!(a, HostId::from_connection_key("ssh-direct:me@box:2222"));
        assert_ne!(a, HostId::from_connection_key("wsl:Ubuntu"));
    }

    #[test]
    fn default_join_uses_the_given_separator() {
        assert_eq!(
            default_join(Path::new("/home/me"), "src", '/'),
            PathBuf::from("/home/me/src")
        );
        assert_eq!(
            default_join(Path::new("/"), "etc", '/'),
            PathBuf::from("/etc")
        );
        assert_eq!(
            default_join(Path::new("/home/me/"), "src", '/'),
            PathBuf::from("/home/me/src")
        );
        assert_eq!(
            default_join(Path::new(r"C:\Users"), "me", '\\'),
            PathBuf::from(r"C:\Users\me")
        );
    }

    #[test]
    fn mtime_handles_both_sides_of_the_epoch() {
        use std::time::{Duration, UNIX_EPOCH};
        let t = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_700);
        assert_eq!(
            MTime::from_system_time(t),
            MTime {
                secs: 1_700_000_000,
                nanos: 123_456_700
            }
        );
        assert_eq!(
            MTime::from_system_time(UNIX_EPOCH),
            MTime { secs: 0, nanos: 0 }
        );
        let before = UNIX_EPOCH - Duration::new(1, 500_000_000);
        assert_eq!(
            MTime::from_system_time(before),
            MTime {
                secs: -2,
                nanos: 500_000_000
            }
        );
    }

    #[test]
    fn output_success_is_exit_zero_only() {
        let ok = Output {
            status: Some(0),
            stdout: b"  main\n".to_vec(),
            stderr: Vec::new(),
        };
        assert!(ok.success());
        assert_eq!(ok.stdout_trimmed(), "main");
        let bad = Output {
            status: Some(128),
            stdout: Vec::new(),
            stderr: b"not a git repository\n".to_vec(),
        };
        assert!(!bad.success());
        assert_eq!(bad.stderr_trimmed(), "not a git repository");
        let signalled = Output {
            status: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(!signalled.success());
    }

    #[test]
    fn output_text_is_lossy_not_fallible() {
        let o = Output {
            status: Some(0),
            stdout: vec![0xff, b'a', 0xfe],
            stderr: Vec::new(),
        };
        assert!(o.stdout_trimmed().contains('a'));
    }

    #[test]
    fn output_bytes_cross_json_as_base64() {
        let o = Output {
            status: Some(1),
            stdout: vec![0x00, 0xff, 0x80, b'h', b'i'],
            stderr: b"boom".to_vec(),
        };
        let json = serde_json::to_string(&o).unwrap();
        assert!(json.contains("\"stdout\":\"AP+AaGk=\""), "{json}");
        assert!(
            !json.contains('['),
            "bytes must not render as an array: {json}"
        );
        assert_eq!(serde_json::from_str::<Output>(&json).unwrap(), o);
    }

    #[test]
    fn value_types_round_trip_through_json() {
        let e = Entry {
            name: "src".into(),
            is_dir: true,
            is_symlink: false,
            ignored: false,
        };
        let back: Entry = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back, e);

        let m = Meta {
            is_dir: false,
            is_symlink: true,
            len: 42,
            mtime: Some(MTime {
                secs: -1,
                nanos: 999_999_999,
            }),
            readonly: true,
        };
        let back: Meta = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);

        let h = SearchHit {
            name: "a.rs".into(),
            path: PathBuf::from("/tmp/a.rs"),
            is_dir: false,
            ignored: true,
        };
        let back: SearchHit = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn the_ui_guard_is_inert_without_registration() {
        guard_off_ui();
    }

    #[test]
    fn host_is_object_safe() {
        fn takes_dyn(_h: &dyn Host) {}
        let h: SharedHost = local::LocalHost::new();
        takes_dyn(&*h);
    }
}
