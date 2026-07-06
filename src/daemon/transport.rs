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
//!   Loopback is reachable by *any* local process, not just the same user — so,
//!   unlike a Unix socket, the port alone isn't an access boundary. The daemon
//!   closes that gap with a token: `bind` writes a random 256-bit token into the
//!   (user-private) port file, `connect` presents it as a preamble, and
//!   `authenticate` rejects any connection that doesn't match — so only a process
//!   that could read the user-private file gets in. See [`imp_windows`].
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

    /// Daemon-side connection authentication — a no-op on Unix. The socket lives in
    /// the user-private config dir (or `$XDG_RUNTIME_DIR`, 0700), so filesystem
    /// permissions already restrict `connect` to the same user; there's nothing to
    /// verify. Mirrors the Windows signature so `server` calls it unconditionally.
    #[inline]
    pub fn authenticate(_stream: &mut Stream) -> io::Result<()> {
        Ok(())
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
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::OnceLock;

    /// The connection stream both sides read/write framed messages over.
    pub type Stream = TcpStream;
    /// The daemon's accept side.
    pub type Listener = TcpListener;

    /// Length of the per-daemon auth token, in bytes. 256 bits from the OS CSPRNG:
    /// unguessable without reading the (user-private) port file, so possessing it
    /// proves the connecting process runs as the same user.
    const TOKEN_LEN: usize = 32;
    type Token = [u8; TOKEN_LEN];

    /// This daemon's auth token, minted once at [`bind`] and checked by
    /// [`authenticate`] on every accepted connection. A process global because the
    /// listener and the per-connection auth check live in the same daemon process
    /// but don't share a handle; the client learns the token from the port file
    /// instead. Set exactly once per daemon lifetime.
    static DAEMON_TOKEN: OnceLock<Token> = OnceLock::new();

    /// Mint a fresh 256-bit token from the OS CSPRNG. Panics only if the OS RNG is
    /// unavailable, which on Windows means the system is too broken to run.
    fn make_token() -> Token {
        let mut token = [0u8; TOKEN_LEN];
        getrandom::fill(&mut token).expect("OS RNG (BCryptGenRandom) unavailable");
        token
    }

    /// Lowercase-hex encode a token for the (text) port file.
    fn encode_token(token: &Token) -> String {
        let mut s = String::with_capacity(TOKEN_LEN * 2);
        for b in token {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
        }
        s
    }

    /// Decode a hex token; `None` unless it's exactly `TOKEN_LEN` bytes of valid hex.
    fn decode_token(s: &str) -> Option<Token> {
        let s = s.trim();
        if s.len() != TOKEN_LEN * 2 {
            return None;
        }
        let bytes = s.as_bytes();
        let mut token = [0u8; TOKEN_LEN];
        for (i, slot) in token.iter_mut().enumerate() {
            let hi = (bytes[i * 2] as char).to_digit(16)?;
            let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
            *slot = ((hi << 4) | lo) as u8;
        }
        Some(token)
    }

    /// The port file records `<port>\n<token-hex>`: the loopback port the GUI
    /// connects to, plus the token it must present. Parse both back; `None` if the
    /// file is malformed (a truncated write, or an old single-line file).
    fn parse_port_file(contents: &str) -> Option<(u16, Token)> {
        let mut lines = contents.lines();
        let port = lines.next()?.trim().parse::<u16>().ok()?;
        let token = decode_token(lines.next()?)?;
        Some((port, token))
    }

    /// Constant-time token comparison: fold every byte's difference into one
    /// accumulator so the check can't leak how many leading bytes matched. A local
    /// timing side-channel is far-fetched over loopback, but the guard is free.
    fn tokens_match(a: &Token, b: &Token) -> bool {
        let mut diff = 0u8;
        for i in 0..TOKEN_LEN {
            diff |= a[i] ^ b[i];
        }
        diff == 0
    }

    /// Path of the port file recording the daemon's chosen loopback port + token.
    /// This is the Windows analogue of the Unix socket file: its presence is the
    /// "endpoint exists" marker, and — being under the user-private config dir —
    /// its contents (the token) are readable only by the same user.
    fn port_path() -> Option<PathBuf> {
        config::config_path("daemon.port")
    }

    /// Read the recorded loopback port + token, if the port file exists and parses.
    fn read_port_file() -> Option<(u16, Token)> {
        let path = port_path()?;
        let contents = std::fs::read_to_string(path).ok()?;
        parse_port_file(&contents)
    }

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }

    /// Try to connect to the daemon. `Err` (including a missing/zero port or a
    /// malformed file) means "nobody home" — the caller treats any error as "not
    /// running". On success we send the auth token as the connection preamble,
    /// before any `ClientMsg`, so the daemon accepts us.
    pub fn connect() -> io::Result<Stream> {
        let (port, token) = read_port_file()
            .filter(|(p, _)| *p != 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no daemon port file"))?;
        let mut stream = TcpStream::connect(loopback(port))?;
        tune(&stream);
        // Present the token first thing; the daemon reads exactly these bytes in
        // `authenticate` before it looks for a `ClientMsg`.
        stream.write_all(&token)?;
        Ok(stream)
    }

    /// Daemon side: read and verify the connection preamble against this daemon's
    /// token before any message is processed. Any process on the machine can open
    /// a loopback TCP connection, but only one that read the user-private port file
    /// knows the token — so this is what makes the loopback endpoint per-user
    /// private, the property a Unix socket gets for free from filesystem perms.
    ///
    /// A short read (peer hung up), a mismatch, or an uninitialized token all fail
    /// the connection; the caller drops it.
    pub fn authenticate(stream: &mut Stream) -> io::Result<()> {
        let expected = DAEMON_TOKEN
            .get()
            .ok_or_else(|| io::Error::other("daemon auth token not initialized"))?;
        authenticate_with(stream, expected)
    }

    /// Pure core of [`authenticate`]: read a token off `reader` and compare it to
    /// `expected`. Split out so the handshake is testable without a live daemon or
    /// the process-global token.
    fn authenticate_with(reader: &mut impl Read, expected: &Token) -> io::Result<()> {
        let mut got = [0u8; TOKEN_LEN];
        reader.read_exact(&mut got)?;
        if tokens_match(&got, expected) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "daemon auth token mismatch",
            ))
        }
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

    /// Bind a loopback listener on an OS-assigned port and record that port — plus
    /// this daemon's freshly-minted auth token — in the port file so the GUI can
    /// find *and* authenticate to it. Ensures the config dir exists first.
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
        // Mint the token once for this daemon's lifetime; `authenticate` checks
        // against the same value. Written to the port file so a client that can
        // read it (same user) can present it back.
        let token = DAEMON_TOKEN.get_or_init(make_token);
        let contents = format!("{port}\n{}", encode_token(token));
        std::fs::write(&path, contents)
            .map_err(|e| anyhow::anyhow!("could not write port file {}: {e}", path.display()))?;
        Ok(listener)
    }

    /// A human-readable description of the endpoint, for log messages.
    pub fn endpoint_display() -> String {
        match read_port_file() {
            Some((port, _)) => format!("127.0.0.1:{port}"),
            None => "127.0.0.1:<unbound>".to_string(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A token round-trips through hex encode → decode unchanged.
        #[test]
        fn token_hex_round_trips() {
            let token = make_token();
            assert_eq!(decode_token(&encode_token(&token)), Some(token));
        }

        /// `decode_token` rejects anything that isn't exactly 32 bytes of hex.
        #[test]
        fn decode_token_rejects_malformed() {
            assert!(decode_token("").is_none());
            assert!(decode_token("zz").is_none());
            assert!(decode_token(&"a".repeat(63)).is_none()); // odd/short
            assert!(decode_token(&"a".repeat(66)).is_none()); // too long
            assert!(decode_token(&"g".repeat(64)).is_none()); // non-hex digit
            assert!(decode_token(&"ab".repeat(32)).is_some()); // exactly right
        }

        /// The port file format is `<port>\n<token-hex>`, and parsing recovers both.
        #[test]
        fn parse_port_file_recovers_port_and_token() {
            let token = make_token();
            let contents = format!("54321\n{}", encode_token(&token));
            assert_eq!(parse_port_file(&contents), Some((54321, token)));
        }

        /// A single-line (legacy / truncated) file has no token, so it must not
        /// parse — a client can't authenticate without one.
        #[test]
        fn parse_port_file_rejects_missing_token() {
            assert!(parse_port_file("54321").is_none());
            assert!(parse_port_file("54321\n").is_none());
            assert!(parse_port_file("").is_none());
            assert!(parse_port_file("notaport\ndeadbeef").is_none());
        }

        /// `tokens_match` is true only for identical tokens.
        #[test]
        fn tokens_match_is_exact() {
            let a = make_token();
            let mut b = a;
            assert!(tokens_match(&a, &b));
            b[TOKEN_LEN - 1] ^= 1; // flip the last bit
            assert!(!tokens_match(&a, &b));
        }

        /// The handshake core accepts the matching token and rejects a wrong one
        /// (and a short read), driven over an in-memory reader — no live daemon.
        #[test]
        fn authenticate_with_accepts_only_the_matching_token() {
            let token = make_token();

            // Correct token → Ok.
            let mut good = std::io::Cursor::new(token.to_vec());
            assert!(authenticate_with(&mut good, &token).is_ok());

            // Wrong token → PermissionDenied.
            let mut wrong_bytes = token;
            wrong_bytes[0] ^= 0xff;
            let mut wrong = std::io::Cursor::new(wrong_bytes.to_vec());
            let err = authenticate_with(&mut wrong, &token).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

            // Short preamble (peer hung up mid-token) → error, never a false accept.
            let mut short = std::io::Cursor::new(vec![0u8; TOKEN_LEN - 1]);
            assert!(authenticate_with(&mut short, &token).is_err());
        }

        /// End-to-end over a real loopback socket: a client that presents the
        /// token authenticates; one that presents garbage is rejected. This is the
        /// exact property the whole change exists to enforce.
        #[test]
        fn loopback_handshake_authenticates_real_connection() {
            let token = make_token();
            let listener = TcpListener::bind(loopback(0)).expect("bind loopback");
            let port = listener.local_addr().unwrap().port();

            // Good client: connect and present the correct token.
            let good = std::thread::spawn(move || {
                let mut s = TcpStream::connect(loopback(port)).unwrap();
                s.write_all(&token).unwrap();
                s
            });
            let (mut server_side, _) = listener.accept().unwrap();
            assert!(authenticate_with(&mut server_side, &token).is_ok());
            let _keep = good.join().unwrap();

            // Bad client: connect and present a wrong token.
            let mut bad_token = token;
            bad_token[5] ^= 0xff;
            let bad = std::thread::spawn(move || {
                let mut s = TcpStream::connect(loopback(port)).unwrap();
                let _ = s.write_all(&bad_token);
            });
            let (mut server_side2, _) = listener.accept().unwrap();
            assert!(authenticate_with(&mut server_side2, &token).is_err());
            bad.join().unwrap();
        }

        /// Full wiring over the real config-dir path: `bind` writes a parseable
        /// `<port>\n<token>` file and seeds the process token, and the public
        /// `authenticate` (which reads that process token) then accepts a client
        /// that presents the file's token. Exercises the `bind`→`connect`→
        /// `authenticate` seam the daemon actually runs, not just the pure core.
        #[test]
        fn bind_seeds_token_and_public_authenticate_accepts_a_file_token_client() {
            // Pin the config dir under a temp dir so the port file never touches the
            // real `%APPDATA%`. First-call-wins, matching the Unix IO tests.
            let dir = std::env::temp_dir().join(format!("tty7-wintok-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok();
            config::set_config_dir(dir);
            remove_stale_endpoint();

            let listener = bind().expect("bind under temp config dir");
            let bound_port = listener.local_addr().unwrap().port();

            // The port file parses and matches the bound port.
            let contents = std::fs::read_to_string(port_path().unwrap()).unwrap();
            let (port, token) = parse_port_file(&contents).expect("port file parses");
            assert_eq!(port, bound_port, "file records the actually-bound port");

            // A client that read the file (has the token) authenticates via the
            // public path, which checks against the token `bind` seeded.
            let good = std::thread::spawn(move || {
                let mut s = TcpStream::connect(loopback(port)).unwrap();
                s.write_all(&token).unwrap();
                s
            });
            let (mut server_side, _) = listener.accept().unwrap();
            assert!(authenticate(&mut server_side).is_ok());
            let _keep = good.join().unwrap();

            remove_stale_endpoint();
        }
    }
}
