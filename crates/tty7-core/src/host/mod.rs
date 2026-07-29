//! [`Host`]: the machine a workspace's files and git live on.
//!
//! Every filesystem read, every write, every `git` shell-out and every file
//! watch tty7 performs on behalf of a workspace goes through this one trait, so
//! that "the files are on this laptop" and "the files are on a box in another
//! datacentre" differ by which `Arc<dyn Host>` the workspace holds and by
//! nothing else. [`local::LocalHost`] is the implementation that answers with
//! `std::fs`; `host::remote::RemoteHost` answers over the control connection;
//! and both are checked against the same [`conformance`] suite, because a
//! difference between them is a bug that only shows up on someone else's
//! machine.
//!
//! # Blocking on purpose
//!
//! Every method blocks. That is a decision, not an oversight: the trait
//! has to be
//! object-safe because the whole tree holds `Arc<dyn Host>`, the server side
//! serves these same calls from a blocking thread pool, and a GPUI
//! `&mut Context<T>` cannot be held across an `.await` anyway — so making the
//! trait async would box every `LocalHost::stat` (the 99% path) without saving
//! a single call site from being restructured.
//!
//! The consequence is a rule: **no `Host` method may be called on the UI
//! thread.** The GUI reaches a host only through `ui::host_ops::HostOps`, which
//! does the `spawn` → `background_spawn` → `update` dance. [`guard_off_ui`]
//! turns a violation into a debug-build panic at the call site rather than a
//! dropped frame nobody can attribute.
//!
//! # Paths belong to the host, not to `std::path`
//!
//! A Windows client talking to a Linux host has to build `/home/me/src`, but
//! `PathBuf::join` would give it `/home/me\src` and `Path::is_absolute` would
//! call `/home/me` relative. So path arithmetic that crosses the boundary goes
//! through [`Host::join`] and [`Host::is_absolute`], which answer with the
//! *host's* semantics. `parent`, `file_name`, `starts_with` and friends are
//! fine as-is — Windows' `std::path` already treats `/` as a separator.

pub mod conformance;
pub mod local;
pub mod remote;
pub mod server;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread::ThreadId;

pub use crate::core::shells::ShellInventory;

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// A stable, in-process identifier for one `Arc<dyn Host>`.
///
/// **Never persisted.** It exists so that structures which cannot hold an
/// `Arc<dyn Host>` — the git-status cache's path tables, pane records, the
/// in-flight maps — can still say *which* machine a path belongs to, and so
/// that two identical paths on two different machines never collide in one map.
///
/// [`HostId::LOCAL`] is `0` and always means this machine. Remote ids are
/// derived from the **connection**, not the workspace: several workspaces on
/// one remote box share an id, matching the granularity at which the SSH
/// connection itself is shared.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct HostId(pub u64);

impl HostId {
    /// This machine. Reserved: no derivation ever produces it.
    pub const LOCAL: HostId = HostId(0);

    /// Derive an id from a normalized connection key.
    ///
    /// `key` must be the canonical connection string for the machine
    /// (`ssh-profile:<uuid>`, `ssh-alias:<alias>`, `ssh-direct:<user>@<host>:<port>`,
    /// `wsl:<distro>`) so that two references to the same box always hash the
    /// same. A hash of exactly `0` is bumped to `1`, because `0` is local's.
    pub fn from_connection_key(key: &str) -> HostId {
        let h = fnv1a64(key.as_bytes());
        HostId(if h == 0 { 1 } else { h })
    }

    /// Whether this is [`HostId::LOCAL`].
    pub fn is_local(self) -> bool {
        self == HostId::LOCAL
    }
}

/// Deterministic 64-bit FNV-1a — the same one `daemon::transport` keys its
/// fallback socket path with.
///
/// Not `DefaultHasher`: ids derived here are compared against ids derived by a
/// *different build* of tty7 (a daemon outlives an app upgrade; a remote server
/// is its own binary), so the function has to be stable across compiler and
/// std versions, which `DefaultHasher` explicitly is not.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// The UI-thread guard
// ---------------------------------------------------------------------------

static UI_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// Record the calling thread as the UI thread, so [`guard_off_ui`] has
/// something to compare against. Idempotent; later calls are ignored.
///
/// `ui::host_ops` calls this on its way through, which is enough: everything it
/// runs on runs on the UI thread by construction, and until it is called the
/// guard simply never fires (a headless `tty7-server` has no UI thread and
/// wants none of this).
pub fn register_ui_thread() {
    let _ = UI_THREAD.set(std::thread::current().id());
}

/// Whether the calling thread is the one [`register_ui_thread`] claimed.
pub fn is_ui_thread() -> bool {
    UI_THREAD
        .get()
        .is_some_and(|t| *t == std::thread::current().id())
}

/// Panic (debug builds only) if a blocking `Host` call is happening on the UI
/// thread.
///
/// Deliberately not `#[cfg(debug_assertions)]` on the *function* — call sites
/// would then need their own `cfg`, and one forgotten `cfg` is one unguarded
/// method. `debug_assert!` already compiles the check away in release.
#[inline]
pub fn guard_off_ui() {
    debug_assert!(
        !is_ui_thread(),
        "Host call on the UI thread — route it through ui::host_ops::HostOps"
    );
}

// ---------------------------------------------------------------------------
// Value types
// ---------------------------------------------------------------------------

/// One entry of a directory listing.
///
/// **No `path` field, on purpose.** The caller rebuilds it with
/// [`Host::join`]: a remote entry's path uses the *remote's* separator, and a
/// `PathBuf` assembled on a Windows client would use a backslash the remote has
/// never heard of.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// The file name, lossily decoded when the host's filesystem holds bytes
    /// that are not valid UTF-8.
    pub name: String,
    /// Whether the entry *resolves to* a directory — symlinks followed, so a
    /// link to a directory is `true` here and `true` in `is_symlink` both.
    pub is_dir: bool,
    /// Whether the entry is itself a symbolic link.
    pub is_symlink: bool,
    /// The host's own gitignore verdict, computed against the chain of
    /// `.gitignore` files from the listing's `root` down. `.git` is always
    /// `true`. Always `false` when the listing had no `root`, or when the host
    /// has no git.
    pub ignored: bool,
}

/// What a `stat` answers.
///
/// Deliberately not `std::fs::Metadata`, which can be neither constructed nor
/// serialized — a remote host has to be able to hand one across a wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    /// Whether the path resolves to a directory (symlinks followed).
    pub is_dir: bool,
    /// Whether the path is itself a symbolic link.
    pub is_symlink: bool,
    /// Size in bytes.
    pub len: u64,
    /// `None` when the platform or filesystem has no modification time.
    pub mtime: Option<MTime>,
    /// Whether the permission bits say read-only.
    pub readonly: bool,
}

/// A modification time, to the nanosecond.
///
/// Nanoseconds rather than milliseconds because the code editor detects
/// external edits by asking "is the mtime still the one I wrote?" — at
/// millisecond granularity a real edit landing in the same millisecond as our
/// own write is indistinguishable from our own write, and gets swallowed.
/// Two fields rather than a `u128` because JSON cannot carry a `u128` without
/// losing precision, and this type crosses a JSON wire.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct MTime {
    /// Whole seconds since the Unix epoch; negative before 1970.
    pub secs: i64,
    /// Nanoseconds within the second, `0..1_000_000_000`.
    pub nanos: u32,
}

impl MTime {
    /// Convert from a `SystemTime`, keeping pre-epoch times exact rather than
    /// clamping them to zero.
    pub fn from_system_time(t: std::time::SystemTime) -> MTime {
        match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => MTime {
                secs: d.as_secs() as i64,
                nanos: d.subsec_nanos(),
            },
            Err(e) => {
                // Before the epoch: `duration_since` hands back how far before.
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

/// What one child-process run produced.
///
/// Deliberately not `std::process::Output`: its `ExitStatus` cannot be
/// constructed portably, and a remote host has to synthesize one from a wire
/// message.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Output {
    /// The exit code, or `None` when the process was killed by a signal or the
    /// code could not be obtained.
    pub status: Option<i32>,
    /// Raw stdout, base64 on the wire.
    ///
    /// Not a plain `Vec<u8>`: `serde_json` has no byte type and renders one as
    /// an array of decimal numbers, so a 1 MB `git diff` would cross the wire as
    /// roughly 4 MB of JSON. (`serde_bytes` does not help here — it forwards to
    /// `serialize_bytes`, which `serde_json` implements as exactly that array.)
    #[serde(with = "b64")]
    pub stdout: Vec<u8>,
    /// Raw stderr, same encoding.
    #[serde(with = "b64")]
    pub stderr: Vec<u8>,
}

/// `Vec<u8>` ⇄ base64 string, for the byte fields that cross a JSON wire.
mod b64 {
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
    /// Exited zero.
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    /// stdout as lossy UTF-8, trimmed — the shape every git call site wants.
    pub fn stdout_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }

    /// stderr as lossy UTF-8, trimmed — the shape error messages want.
    pub fn stderr_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

/// One hit from [`Host::search`]. Unlike [`Entry`] this *does* carry a path:
/// hits come from directories the caller never listed, so there is nothing to
/// join against — the host, which knows its own separator, builds it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    /// The file name that matched.
    pub name: String,
    /// The absolute path, in the host's own separator.
    pub path: PathBuf,
    /// Whether the hit is a directory.
    pub is_dir: bool,
    /// Whether the hit is gitignored.
    pub ignored: bool,
}

// ---------------------------------------------------------------------------
// Watching
// ---------------------------------------------------------------------------

/// A live subscription to filesystem changes.
///
/// Long-lived and *mutable*: the file tree changes which directories it cares
/// about every time a row is expanded, and tearing the subscription down and
/// rebuilding it would cost a round trip plus a rebuilt server-side watcher for
/// every disclosure triangle. So the subscription outlives the set, and
/// [`WatchSub::set_dirs`] replaces the set in place.
///
/// Dropping it unsubscribes.
pub struct WatchSub {
    rx: smol::channel::Receiver<Vec<PathBuf>>,
    inner: Box<dyn WatchHandle>,
}

impl WatchSub {
    /// Build a subscription from its two halves. Implementations of
    /// [`Host::watch`] call this; nothing else needs to.
    pub fn new(rx: smol::channel::Receiver<Vec<PathBuf>>, inner: Box<dyn WatchHandle>) -> WatchSub {
        WatchSub { rx, inner }
    }

    /// The batched event stream.
    ///
    /// Batches are coalesced over a 100ms window and deduplicated within it —
    /// **by every implementation, identically**. A local watcher is not allowed
    /// to be "helpfully" more immediate than a remote one, because then the
    /// consumer's idea of how often it repaints would depend on where the files
    /// happen to live.
    pub fn events(&self) -> &smol::channel::Receiver<Vec<PathBuf>> {
        &self.rx
    }

    /// Replace the watched set wholesale. The implementation works out the
    /// difference; the caller only ever states the full set.
    ///
    /// Always **non-recursive**: a directory being watched says nothing about
    /// its subdirectories.
    pub fn set_dirs(&self, dirs: &[PathBuf]) -> io::Result<()> {
        self.inner.set_dirs(dirs)
    }
}

/// The implementation half of a [`WatchSub`]: whatever has to be told when the
/// watched set changes, and whatever has to be torn down on drop.
pub trait WatchHandle: Send + Sync {
    /// Replace the watched directory set.
    fn set_dirs(&self, dirs: &[PathBuf]) -> io::Result<()>;
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

/// A machine's filesystem and git, behind one blocking, object-safe interface.
///
/// See the module docs for why it blocks and why paths go through
/// [`join`](Host::join) / [`is_absolute`](Host::is_absolute) rather than
/// `std::path`.
pub trait Host: Send + Sync + 'static {
    // ----- identity --------------------------------------------------------

    /// This host's in-process id.
    fn id(&self) -> HostId;

    /// The path separator this host's filesystem uses: the platform's own for a
    /// local host, `/` for a remote Linux or WSL one.
    fn separator(&self) -> char;

    // ----- path arithmetic -------------------------------------------------

    /// `dir` + `name`, using this host's separator.
    ///
    /// Use this instead of `Path::join` for any path that might belong to a
    /// remote host — `PathBuf::join` uses the *client's* separator, which on a
    /// Windows client talking to Linux produces `/home/me\src`.
    fn join(&self, dir: &Path, name: &str) -> PathBuf {
        default_join(dir, name, self.separator())
    }

    /// Whether `p` is absolute *in this host's semantics*.
    ///
    /// Use this instead of `Path::is_absolute`: on a Windows client
    /// `Path::new("/home/me").is_absolute()` is `false` (it reads as
    /// drive-relative), which would silently mis-classify every remote POSIX
    /// path.
    fn is_absolute(&self, p: &Path) -> bool;

    // ----- reading ---------------------------------------------------------

    /// List `dir`, **already sorted**: directories first, then case-insensitive
    /// by name. Sorting is the host's job so that a remote listing arrives
    /// ready to render and the order can never drift between hosts.
    ///
    /// `root` bounds the gitignore chain: each entry's `ignored` is scored by
    /// walking `.gitignore` files from `root` down to `dir`, deepest match
    /// winning and `!` whitelists un-ignoring. With `root == None` nothing is
    /// ignored except `.git` itself.
    ///
    /// Hidden files are **not** filtered — "show hidden" is a UI preference and
    /// stays on the client.
    fn read_dir(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<Entry>>;

    /// Metadata for `p`, symlinks followed.
    fn stat(&self, p: &Path) -> io::Result<Meta>;

    /// Whether `p` exists. Separate from `stat` so an implementation can answer
    /// in one round trip instead of shipping metadata nobody asked for.
    fn exists(&self, p: &Path) -> bool {
        self.stat(p).is_ok()
    }

    /// Read `p` whole.
    ///
    /// `max_bytes` is enforced **by the host**: a file over the limit fails with
    /// [`io::ErrorKind::FileTooLarge`] without its contents being transferred,
    /// rather than being shipped across an ocean and then discarded.
    fn read_file(&self, p: &Path, max_bytes: u64) -> io::Result<Vec<u8>>;

    /// Resolve `p` to an absolute path with symlinks and `..` resolved, on the
    /// host's own filesystem.
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf>;

    /// Breadth-first substring search over file names, **executed on the
    /// host**.
    ///
    /// Running this client-side would mean up to `max_dirs` separate directory
    /// listings; at 200ms of round trip each that is a search which takes
    /// minutes. So the whole walk goes to the host and only the hits come back.
    ///
    /// The walk starts at each of `roots`, visits at most `max_dirs`
    /// directories in total, stops at `limit` hits, and — when `show_hidden` is
    /// false — never descends into an ignored or dot-prefixed directory, which
    /// is what keeps `node_modules` and `target` from eating the whole budget.
    fn search(
        &self,
        roots: &[PathBuf],
        query: &str,
        limit: usize,
        max_dirs: usize,
        show_hidden: bool,
    ) -> io::Result<Vec<SearchHit>>;

    // ----- writing ---------------------------------------------------------

    /// Write `bytes` to `p`, creating or truncating it, and answer the file's
    /// post-write [`Meta`]. A missing parent directory is an error, not
    /// something to create.
    ///
    /// **Why it returns `Meta` rather than `()`.** The editor keeps a
    /// `disk_mtime` baseline to tell its own write apart from someone else's
    /// edit. Taking that baseline from a *separate* `stat` after the write
    /// leaves a window: a change landing in between is stamped as ours, and the
    /// editor then never reports it — silent, and it costs the user their
    /// conflict prompt. The post-write metadata is the write's own answer, so
    /// it closes the window by construction. It is also one round trip instead
    /// of two on every remote save; the control reply already carried it
    ///, so nothing on the wire moved.
    ///
    /// Callers that genuinely don't want it write `.map(|_| ())`.
    fn write_file(&self, p: &Path, bytes: &[u8]) -> io::Result<Meta>;

    /// Create `p` as an empty file, failing with
    /// [`io::ErrorKind::AlreadyExists`] if anything is already there.
    fn create_file_new(&self, p: &Path) -> io::Result<()>;

    /// Create directory `p`. With `recursive`, create missing parents too and
    /// treat an existing directory as success.
    fn create_dir(&self, p: &Path, recursive: bool) -> io::Result<()>;

    /// Move `from` to `to`.
    ///
    /// An existing `to` is [`io::ErrorKind::AlreadyExists`], **guaranteed by
    /// the implementation** — a caller that probed first would be paying an
    /// extra round trip for a check that is racy anyway.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Remove `p`. `recursive` only means anything for a directory; a
    /// non-empty directory without it is
    /// [`io::ErrorKind::DirectoryNotEmpty`].
    fn remove(&self, p: &Path, recursive: bool) -> io::Result<()>;

    // ----- git -------------------------------------------------------------

    /// The work-tree root `p` belongs to: the nearest ancestor holding a `.git`
    /// (a directory, or the file a linked worktree gets). `Ok(None)` — not an
    /// error — when `p` is outside any repository.
    ///
    /// The whole ancestor walk happens on the host; a remote implementation
    /// must not climb one level per round trip.
    fn repo_root(&self, p: &Path) -> io::Result<Option<PathBuf>>;

    /// Run `git -C <cwd> <args>` on the host.
    ///
    /// `Ok` means git *ran*; its exit code is in [`Output::status`], and a
    /// non-zero one is a perfectly ordinary answer (`rev-parse` outside a repo
    /// exits 128). `Err` means it could not be run at all — no git, missing
    /// `cwd`, connection gone.
    ///
    /// Every implementation runs it under the same invariants: `-C` rather than
    /// a current directory, `GIT_OPTIONAL_LOCKS=0`, null stdin, `GIT_DIR` and
    /// `GIT_WORK_TREE` cleared, and both output streams captured.
    fn git(&self, cwd: &Path, args: &[&str]) -> io::Result<Output>;

    // ----- machine inventory -----------------------------------------------

    /// The shells this host can launch, plus which one a plain new tab lands
    /// on — the new-tab dropdown's menu.
    ///
    /// On the trait rather than beside `detect_shells` because a window bound to
    /// a remote workspace opens its tabs *over there*: a picker built from this
    /// computer's `/etc/shells` offers paths that don't exist on the machine the
    /// spawn actually reaches. Probing is not free (Windows enumerates WSL by
    /// spawning `wsl.exe`), so callers ask once per machine, not per menu open.
    fn shells(&self) -> io::Result<ShellInventory>;

    // ----- watching --------------------------------------------------------

    /// Open a long-lived, non-recursive watch over `dirs` (which may be empty —
    /// the set can be filled in later with [`WatchSub::set_dirs`]).
    fn watch(&self, dirs: &[PathBuf]) -> io::Result<WatchSub>;

    // ----- liveness --------------------------------------------------------

    /// Whether the host is reachable right now.
    ///
    /// Always true locally. A remote host reports false while reconnecting or
    /// after being taken over, and call sites use that to keep showing the last
    /// good listing instead of flashing an error.
    fn is_connected(&self) -> bool {
        true
    }
}

/// Join `name` onto `dir` with an explicit separator — the default
/// [`Host::join`], and the one a remote host uses.
pub fn default_join(dir: &Path, name: &str, sep: char) -> PathBuf {
    let mut s = dir.to_string_lossy().into_owned();
    if !s.is_empty() && !s.ends_with(sep) && !s.ends_with('/') {
        s.push(sep);
    }
    s.push_str(name);
    PathBuf::from(s)
}

/// The alias the rest of the tree uses. A workspace holds one of these; nothing
/// holds a concrete host type.
pub type SharedHost = Arc<dyn Host>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash has to stay bit-for-bit what `daemon::transport` computes, or an
    /// upgraded client would derive different ids than the daemon it is talking
    /// to. Pinned against the published FNV-1a-64 vectors rather than against
    /// our own output, so a "refactor" that changes the algorithm fails here.
    #[test]
    fn fnv1a64_matches_the_published_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// `HostId(0)` means local and nothing derived may claim it. Sweeping a few
    /// thousand plausible keys is not a proof, but it is the part of the
    /// reservation that could plausibly regress (someone dropping the `h == 0`
    /// bump as dead code).
    #[test]
    fn zero_is_reserved_for_local() {
        assert!(HostId::LOCAL.is_local());
        for i in 0..2000u32 {
            let key = format!("ssh-direct:me@box{i}:22");
            assert!(!HostId::from_connection_key(&key).is_local());
        }
        assert!(!HostId::from_connection_key("").is_local());
    }

    /// Same machine, same id; different machines, different ids. This is what
    /// keeps two workspaces on one remote box sharing a git-status cache.
    #[test]
    fn connection_keys_map_to_stable_ids() {
        let a = HostId::from_connection_key("ssh-direct:me@box:22");
        assert_eq!(a, HostId::from_connection_key("ssh-direct:me@box:22"));
        assert_ne!(a, HostId::from_connection_key("ssh-direct:me@box:2222"));
        assert_ne!(a, HostId::from_connection_key("wsl:Ubuntu"));
    }

    /// The remote separator wins, whatever the client's `std::path` thinks —
    /// the whole point of not using `PathBuf::join`.
    #[test]
    fn default_join_uses_the_given_separator() {
        assert_eq!(
            default_join(Path::new("/home/me"), "src", '/'),
            PathBuf::from("/home/me/src")
        );
        // Already separated: no doubling.
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

    /// Pre-epoch times round-trip exactly rather than clamping to zero, because
    /// the editor compares mtimes for equality.
    ///
    /// Every nanosecond figure here is a multiple of 100: a Windows
    /// `SystemTime` is a FILETIME, whose tick *is* 100ns, so a finer value
    /// would be rounded on the way in and the assertion would be about
    /// `SystemTime`'s resolution rather than about this conversion.
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

    /// `Err` is "it did not run"; a non-zero exit is an ordinary `Ok`. Every
    /// git call site's error handling is built on that split.
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

    /// Non-UTF-8 output does not lose the run: it comes back lossy rather than
    /// turning the call into an error.
    #[test]
    fn output_text_is_lossy_not_fallible() {
        let o = Output {
            status: Some(0),
            stdout: vec![0xff, b'a', 0xfe],
            stderr: Vec::new(),
        };
        assert!(o.stdout_trimmed().contains('a'));
    }

    /// Arbitrary bytes survive a JSON round trip, and they do it as base64
    /// rather than as an array of numbers — the difference between 1.33× and 4×
    /// on a `git diff` big enough to matter.
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

    /// The listing/metadata types have to survive the same trip, since they are
    /// the payload of every read RPC.
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

    /// The guard is inert until a UI thread claims itself, which is what lets
    /// `tty7-server` — and every test — call hosts from any thread.
    #[test]
    fn the_ui_guard_is_inert_without_registration() {
        // No `register_ui_thread` in the server or in tests, so this is a no-op
        // rather than a panic.
        guard_off_ui();
    }

    /// Object safety is not decoration: the whole tree stores `Arc<dyn Host>`,
    /// and the conformance suite takes `&dyn Host` precisely to keep this true.
    #[test]
    fn host_is_object_safe() {
        fn takes_dyn(_h: &dyn Host) {}
        let h: SharedHost = local::LocalHost::new();
        takes_dyn(&*h);
    }
}
