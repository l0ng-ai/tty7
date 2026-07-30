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

    fn write_file(&self, p: &Path, bytes: &[u8]) -> io::Result<Meta>;

    fn create_file_new(&self, p: &Path) -> io::Result<()>;

    fn create_dir(&self, p: &Path, recursive: bool) -> io::Result<()>;

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    fn remove(&self, p: &Path, recursive: bool) -> io::Result<()>;

    fn repo_root(&self, p: &Path) -> io::Result<Option<PathBuf>>;

    fn git(&self, cwd: &Path, args: &[&str]) -> io::Result<Output>;

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

    fn watch(&self, dirs: &[PathBuf]) -> io::Result<WatchSub>;

    fn is_connected(&self) -> bool {
        true
    }
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
