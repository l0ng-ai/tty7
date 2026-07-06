//! Cross-platform IPC transport for the GUI ⇄ daemon connection.
//!
//! The daemon and the GUI talk over a local, machine-private byte stream. Which
//! kind of stream depends on the platform, but both sides only ever see a type
//! that is `Read + Write + try_clone` — so `server`, `spawn`, and
//! `terminal::remote` share one code path and never mention the concrete type.
//!
//! - **Unix**: a Unix-domain socket at `<config>/daemon.sock`. This is the
//!   original design, kept verbatim — the socket file's presence on disk doubles
//!   as the "is a daemon here?" marker, and `bind` recreates it.
//! - **Windows**: a loopback `TcpListener` on `127.0.0.1:<port>` (an OS-assigned
//!   ephemeral port). Windows has no first-class Unix sockets, and the
//!   `interprocess` named-pipe route can't cleanly `try_clone` a blocking duplex
//!   handle, which our thread-per-connection model needs. Loopback TCP has the
//!   exact `try_clone` + blocking semantics of `UnixStream`, so the rest of the
//!   daemon is unchanged. The chosen port is written to `<config>/daemon.port`
//!   so the GUI can find a daemon it didn't spawn; that file is the Windows
//!   analogue of the socket file (its presence is the "endpoint exists" marker).
//!   Loopback is reachable only by processes on the same machine; a stricter
//!   pipe/ACL story is a P1 hardening (see the Windows-adaptation notes).
//!
//! All endpoint state lives under the (config-dir-aware) config directory, so
//! `--config-dir` / `cargo dev` isolation reaches the daemon on every platform.

use std::io;

use crate::core::config;

#[cfg(unix)]
pub use imp_unix::*;
#[cfg(windows)]
pub use imp_windows::*;

#[cfg(unix)]
mod imp_unix {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    /// The connection stream both sides read/write framed messages over.
    pub type Stream = UnixStream;
    /// The daemon's accept side.
    pub type Listener = UnixListener;

    /// `sockaddr_un.sun_path` caps socket paths at 104 bytes on macOS (108 on
    /// Linux), NUL included — `bind`/`connect` reject anything longer, so stay
    /// safely below the smaller limit.
    pub(super) const MAX_SOCKET_PATH_BYTES: usize = 100;

    /// Deterministic 64-bit FNV-1a. Not `DefaultHasher`: the GUI and the daemon
    /// can be different builds of tty7 (daemon survives app upgrades), so the
    /// fallback socket path must hash identically across compiler/std versions
    /// or an upgraded GUI would lose a live daemon.
    fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }

    /// The socket path serving `config_dir`: `<config_dir>/daemon.sock` whenever
    /// that fits in `sun_path`, else a short per-user path keyed by a stable
    /// hash of the config dir. Without the fallback, a long `--config-dir` made
    /// bind/connect fail with "path must be shorter than SUN_LEN" and the GUI
    /// died at startup. Distinct config dirs still get distinct daemons (the
    /// hash keys the endpoint), and both processes derive the same path because
    /// the GUI forwards its *resolved* config dir to the daemon it spawns.
    pub(super) fn socket_path_for(config_dir: &Path) -> PathBuf {
        use std::os::unix::ffi::OsStrExt as _;
        let inline = config_dir.join("daemon.sock");
        if inline.as_os_str().as_bytes().len() <= MAX_SOCKET_PATH_BYTES {
            return inline;
        }
        // Prefer $XDG_RUNTIME_DIR (user-private, 0700 — the norm on Linux);
        // otherwise the OS temp dir, which is per-user on macOS.
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let hash = fnv1a64(config_dir.as_os_str().as_bytes());
        base.join(format!("tty7-{hash:016x}.sock"))
    }

    /// Path of the Unix-domain socket for this process's config dir. `None` only
    /// when the config dir can't be resolved (no `$HOME`).
    fn socket_path() -> Option<PathBuf> {
        Some(socket_path_for(&config::config_dir_path()?))
    }

    /// Try to connect to the daemon. `Err` means "nobody home" (the caller treats
    /// any error as "not running").
    pub fn connect() -> io::Result<Stream> {
        let path = socket_path().ok_or_else(|| {
            io::Error::other("could not resolve daemon socket path (no config dir)")
        })?;
        let stream = UnixStream::connect(path)?;
        tune(&stream);
        Ok(stream)
    }

    /// Grow the kernel socket buffers to match the daemon writer's 256 KiB
    /// coalesced Output frames. macOS defaults Unix-socket buffers to 8 KiB,
    /// which chops a full-drain stream (100+ MB/s) into ~8 KiB reads — tens of
    /// thousands of extra syscalls and cross-process wakeups per second, and a
    /// stall point the PTY reader's backpressure gate then amplifies. Best
    /// effort: a refused size just keeps the platform default.
    pub fn tune(stream: &Stream) {
        use std::os::unix::io::AsRawFd as _;
        let size: libc::c_int = 256 * 1024;
        for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            // SAFETY: plain setsockopt on a valid owned fd with a c_int payload.
            unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    opt,
                    (&raw const size).cast(),
                    size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
    }

    /// Whether the endpoint marker exists on disk (a live *or* stale socket file).
    pub fn endpoint_exists() -> bool {
        socket_path().is_some_and(|p| p.exists())
    }

    /// Remove a stale endpoint marker so a fresh `bind` can recreate it. Best
    /// effort: a missing file is fine.
    pub fn remove_stale_endpoint() {
        if let Some(path) = socket_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Bind the listener (daemon side). Ensures the config dir exists first; the
    /// caller is responsible for having cleared any stale endpoint.
    pub fn bind() -> anyhow::Result<Listener> {
        let path = socket_path().ok_or_else(|| {
            anyhow::anyhow!("could not resolve daemon socket path (no config dir)")
        })?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let listener = UnixListener::bind(&path)
            .map_err(|e| anyhow::anyhow!("bind {} failed: {}", path.display(), e))?;
        Ok(listener)
    }

    /// A human-readable description of the endpoint, for log messages.
    pub fn endpoint_display() -> String {
        socket_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unresolved>".to_string())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Pin the process config dir so the socket lives under a temp dir, never the
    /// real `~/.config`. First-call-wins; every IO test computes the same path.
    fn pin_config_dir() {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        config::set_config_dir(dir);
    }

    /// One test drives the whole endpoint lifecycle so the shared `daemon.sock`
    /// file isn't raced by parallel tests: clean → bind → exists/connect → remove.
    #[test]
    fn endpoint_lifecycle_bind_connect_and_clear() {
        pin_config_dir();
        // Start from a clean slate (a prior run may have left a stale socket).
        remove_stale_endpoint();
        assert!(!endpoint_exists(), "no endpoint before bind");

        let listener = bind().expect("bind should succeed under the temp config dir");
        assert!(endpoint_exists(), "the socket file marks the endpoint");
        assert!(
            endpoint_display().contains("daemon.sock"),
            "display names the socket file"
        );

        // A client can connect while the listener is alive.
        let _client = connect().expect("connect to the live listener");

        drop(listener);
        // The socket file lingers after the listener drops; clearing it makes the
        // endpoint look absent again (the stale-takeover path in `run`).
        remove_stale_endpoint();
        assert!(!endpoint_exists(), "endpoint cleared after removal");
    }

    /// A short config dir keeps the original `<config>/daemon.sock` layout —
    /// existing daemons must stay reachable across this change.
    #[test]
    fn socket_path_stays_in_config_dir_when_it_fits() {
        let dir = std::path::PathBuf::from("/tmp/tty7-short");
        assert_eq!(imp_unix::socket_path_for(&dir), dir.join("daemon.sock"));
    }

    /// An overlong config dir (the SUN_LEN panic regression) falls back to a
    /// short path that is deterministic and still keyed to the config dir.
    #[test]
    fn socket_path_falls_back_when_config_dir_is_too_long() {
        use std::os::unix::ffi::OsStrExt as _;
        let long_a = std::path::PathBuf::from(format!("/tmp/{}", "a".repeat(150)));
        let long_b = std::path::PathBuf::from(format!("/tmp/{}", "b".repeat(150)));

        let path = imp_unix::socket_path_for(&long_a);
        assert!(
            path.as_os_str().as_bytes().len() <= imp_unix::MAX_SOCKET_PATH_BYTES,
            "fallback path must fit sun_path: {}",
            path.display()
        );
        assert_eq!(
            path,
            imp_unix::socket_path_for(&long_a),
            "GUI and daemon must derive the same endpoint"
        );
        assert_ne!(
            path,
            imp_unix::socket_path_for(&long_b),
            "distinct config dirs keep distinct daemons"
        );
    }

    /// End-to-end on the OS: the fallback path actually binds and accepts a
    /// connection (this is exactly what failed with SUN_LEN before).
    #[test]
    fn fallback_socket_binds_and_connects() {
        use std::os::unix::net::{UnixListener, UnixStream};
        // Pid-keyed so concurrent `cargo test` processes don't share a path.
        let long_dir =
            std::env::temp_dir().join(format!("{}-{}", "x".repeat(120), std::process::id()));
        let path = imp_unix::socket_path_for(&long_dir);
        assert_ne!(
            path.parent(),
            Some(long_dir.as_path()),
            "must not live in the long dir"
        );

        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind fallback socket");
        let _client = UnixStream::connect(&path).expect("connect fallback socket");
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(windows)]
mod imp_windows {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::path::PathBuf;

    /// The connection stream both sides read/write framed messages over.
    pub type Stream = TcpStream;
    /// The daemon's accept side.
    pub type Listener = TcpListener;

    /// Path of the port file recording the daemon's chosen loopback port. This is
    /// the Windows analogue of the Unix socket file: its presence is the
    /// "endpoint exists" marker.
    fn port_path() -> Option<PathBuf> {
        config::config_path("daemon.port")
    }

    /// Read the recorded loopback port, if the port file exists and parses.
    fn read_port() -> Option<u16> {
        let path = port_path()?;
        std::fs::read_to_string(path)
            .ok()?
            .trim()
            .parse::<u16>()
            .ok()
    }

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }

    /// Try to connect to the daemon. `Err` (including a missing/zero port) means
    /// "nobody home" — the caller treats any error as "not running".
    pub fn connect() -> io::Result<Stream> {
        let port = read_port()
            .filter(|p| *p != 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no daemon port file"))?;
        let stream = TcpStream::connect(loopback(port))?;
        tune(&stream);
        Ok(stream)
    }

    /// Loopback-TCP analogue of the Unix `tune`: disable Nagle so small framed
    /// messages (keystrokes, resizes) aren't held back waiting for an ACK.
    /// Buffer sizes are left at the Windows defaults (already 64 KiB). Best
    /// effort.
    pub fn tune(stream: &Stream) {
        let _ = stream.set_nodelay(true);
    }

    /// Whether the endpoint marker (port file) exists on disk.
    pub fn endpoint_exists() -> bool {
        port_path().is_some_and(|p| p.exists())
    }

    /// Remove a stale endpoint marker (the port file). Best effort.
    pub fn remove_stale_endpoint() {
        if let Some(path) = port_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Bind a loopback listener on an OS-assigned port and record that port in the
    /// port file so the GUI can find it. Ensures the config dir exists first.
    pub fn bind() -> anyhow::Result<Listener> {
        let path = port_path()
            .ok_or_else(|| anyhow::anyhow!("could not resolve daemon port path (no config dir)"))?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Port 0 lets the OS pick a free ephemeral port; we read it back so the
        // GUI connects to the actual bound port.
        let listener = TcpListener::bind(loopback(0))
            .map_err(|e| anyhow::anyhow!("bind 127.0.0.1:0 failed: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| anyhow::anyhow!("could not read bound port: {e}"))?
            .port();
        std::fs::write(&path, port.to_string())
            .map_err(|e| anyhow::anyhow!("could not write port file {}: {e}", path.display()))?;
        Ok(listener)
    }

    /// A human-readable description of the endpoint, for log messages.
    pub fn endpoint_display() -> String {
        match read_port() {
            Some(port) => format!("127.0.0.1:{port}"),
            None => "127.0.0.1:<unbound>".to_string(),
        }
    }
}
