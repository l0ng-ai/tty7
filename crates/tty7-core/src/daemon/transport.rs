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

    pub type Stream = UnixStream;
    pub type Listener = UnixListener;

    pub(super) const MAX_SOCKET_PATH_BYTES: usize = 100;

    fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }

    pub(super) fn socket_path_for(config_dir: &Path) -> PathBuf {
        use std::os::unix::ffi::OsStrExt as _;
        let inline = config_dir.join("daemon.sock");
        if inline.as_os_str().as_bytes().len() <= MAX_SOCKET_PATH_BYTES {
            return inline;
        }
        let hash = fnv1a64(config_dir.as_os_str().as_bytes());
        let name = format!("tty7-{hash:016x}.sock");
        let xdg = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from);
        pick_fallback_socket(xdg.as_deref(), &std::env::temp_dir(), &name)
    }

    pub(super) fn pick_fallback_socket(xdg: Option<&Path>, temp: &Path, name: &str) -> PathBuf {
        use std::os::unix::ffi::OsStrExt as _;
        let fits = |p: &PathBuf| p.as_os_str().as_bytes().len() <= MAX_SOCKET_PATH_BYTES;
        let preferred = xdg.unwrap_or(temp).join(name);
        if fits(&preferred) {
            return preferred;
        }
        let temp_path = temp.join(name);
        if fits(&temp_path) {
            return temp_path;
        }
        preferred
    }

    fn socket_path() -> Option<PathBuf> {
        Some(socket_path_for(&config::config_dir_path()?))
    }

    pub fn connect() -> io::Result<Stream> {
        let path = socket_path().ok_or_else(|| {
            io::Error::other("could not resolve daemon socket path (no config dir)")
        })?;
        let stream = UnixStream::connect(path)?;
        tune(&stream);
        Ok(stream)
    }

    pub fn tune(stream: &Stream) {
        use std::os::unix::io::AsRawFd as _;
        let size: libc::c_int = 256 * 1024;
        for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
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

    #[inline]
    pub fn authenticate(_stream: &mut Stream) -> io::Result<()> {
        Ok(())
    }

    pub fn endpoint_exists() -> bool {
        socket_path().is_some_and(|p| p.exists())
    }

    pub fn remove_stale_endpoint() {
        if let Some(path) = socket_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    pub fn bind() -> anyhow::Result<Listener> {
        use std::os::unix::fs::PermissionsExt as _;
        let path = socket_path().ok_or_else(|| {
            anyhow::anyhow!("could not resolve daemon socket path (no config dir)")
        })?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
            let owns_parent = config::config_dir_path().is_some_and(|c| c.as_path() == parent);
            if owns_parent {
                if let Err(e) =
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                {
                    log::warn!(
                        "could not chmod 0700 daemon socket dir {}: {e}",
                        parent.display()
                    );
                }
            }
        }
        let listener = UnixListener::bind(&path)
            .map_err(|e| anyhow::anyhow!("bind {} failed: {}", path.display(), e))?;
        if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
            log::warn!("could not chmod 0600 daemon socket {}: {e}", path.display());
        }
        Ok(listener)
    }

    pub fn endpoint_display() -> String {
        socket_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unresolved>".to_string())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn pin_config_dir() {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        config::set_config_dir(dir);
    }

    #[test]
    fn endpoint_lifecycle_bind_connect_and_clear() {
        pin_config_dir();
        remove_stale_endpoint();
        assert!(!endpoint_exists(), "no endpoint before bind");

        let listener = bind().expect("bind should succeed under the temp config dir");
        assert!(endpoint_exists(), "the socket file marks the endpoint");
        assert!(
            endpoint_display().contains("daemon.sock"),
            "display names the socket file"
        );

        let _client = connect().expect("connect to the live listener");

        drop(listener);
        remove_stale_endpoint();
        assert!(!endpoint_exists(), "endpoint cleared after removal");
    }

    #[test]
    fn socket_path_stays_in_config_dir_when_it_fits() {
        let dir = std::path::PathBuf::from("/tmp/tty7-short");
        assert_eq!(imp_unix::socket_path_for(&dir), dir.join("daemon.sock"));
    }

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

    #[test]
    fn fallback_socket_binds_and_connects() {
        use std::os::unix::net::{UnixListener, UnixStream};
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

    #[test]
    fn a_long_runtime_dir_falls_through_to_the_temp_dir() {
        let name = "tty7-0123456789abcdef.sock";
        let long_xdg = PathBuf::from(format!("/run/user/1000/{}", "d".repeat(90)));
        let temp = PathBuf::from("/tmp");

        let picked = imp_unix::pick_fallback_socket(Some(&long_xdg), &temp, name);
        assert_eq!(picked, temp.join(name), "falls through to the temp dir");

        let short_xdg = PathBuf::from("/run/user/1000");
        assert_eq!(
            imp_unix::pick_fallback_socket(Some(&short_xdg), &temp, name),
            short_xdg.join(name),
            "$XDG_RUNTIME_DIR still wins whenever it fits",
        );
        assert_eq!(
            imp_unix::pick_fallback_socket(None, &temp, name),
            temp.join(name),
            "no runtime dir means the temp dir, as before",
        );
    }

    #[test]
    fn an_unusable_pair_of_bases_still_reports_the_preferred_path() {
        let name = "tty7-0123456789abcdef.sock";
        let long_xdg = PathBuf::from(format!("/run/{}", "d".repeat(90)));
        let long_temp = PathBuf::from(format!("/tmp/{}", "t".repeat(90)));
        assert_eq!(
            imp_unix::pick_fallback_socket(Some(&long_xdg), &long_temp, name),
            long_xdg.join(name),
        );
    }
}

#[cfg(windows)]
mod imp_windows {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::OnceLock;

    pub type Stream = TcpStream;
    pub type Listener = TcpListener;

    pub const TOKEN_LEN: usize = 32;
    pub type Token = [u8; TOKEN_LEN];

    const PANE_PORT_FILE: &str = "daemon.port";

    static DAEMON_TOKEN: OnceLock<Token> = OnceLock::new();

    fn make_token() -> Token {
        let mut token = [0u8; TOKEN_LEN];
        getrandom::fill(&mut token).expect("OS RNG (BCryptGenRandom) unavailable");
        token
    }

    fn encode_token(token: &Token) -> String {
        let mut s = String::with_capacity(TOKEN_LEN * 2);
        for b in token {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
        }
        s
    }

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

    fn parse_port_file(contents: &str) -> Option<(u16, Token)> {
        let mut lines = contents.lines();
        let port = lines.next()?.trim().parse::<u16>().ok()?;
        let token = decode_token(lines.next()?)?;
        Some((port, token))
    }

    fn tokens_match(a: &Token, b: &Token) -> bool {
        let mut diff = 0u8;
        for i in 0..TOKEN_LEN {
            diff |= a[i] ^ b[i];
        }
        diff == 0
    }

    fn port_path() -> Option<PathBuf> {
        port_path_named(PANE_PORT_FILE)
    }

    pub fn port_path_named(file: &str) -> Option<PathBuf> {
        config::config_path(file)
    }

    fn read_port_file() -> Option<(u16, Token)> {
        read_port_file_named(PANE_PORT_FILE)
    }

    fn read_port_file_named(file: &str) -> Option<(u16, Token)> {
        let path = port_path_named(file)?;
        let contents = std::fs::read_to_string(path).ok()?;
        parse_port_file(&contents)
    }

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }

    pub fn connect() -> io::Result<Stream> {
        let (port, token) = read_port_file()
            .filter(|(p, _)| *p != 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no daemon port file"))?;
        let mut stream = TcpStream::connect(loopback(port))?;
        tune(&stream);
        stream.write_all(&token)?;
        Ok(stream)
    }

    pub fn authenticate(stream: &mut Stream) -> io::Result<()> {
        let expected = DAEMON_TOKEN
            .get()
            .ok_or_else(|| io::Error::other("daemon auth token not initialized"))?;
        authenticate_with(stream, expected)
    }

    pub fn check_endpoint_token(stream: &mut Stream, expected: &Token) -> io::Result<()> {
        authenticate_with(stream, expected)
    }

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

    pub fn tune(stream: &Stream) {
        let _ = stream.set_nodelay(true);
    }

    pub fn endpoint_exists() -> bool {
        port_path().is_some_and(|p| p.exists())
    }

    pub fn remove_stale_endpoint() {
        if let Some(path) = port_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    pub fn bind() -> anyhow::Result<Listener> {
        let token = *DAEMON_TOKEN.get_or_init(make_token);
        let (listener, _) = bind_named(PANE_PORT_FILE, token)?;
        Ok(listener)
    }

    pub fn bind_endpoint(file: &str) -> anyhow::Result<(Listener, Token)> {
        bind_named(file, make_token())
    }

    fn bind_named(file: &str, token: Token) -> anyhow::Result<(Listener, Token)> {
        let path = port_path_named(file)
            .ok_or_else(|| anyhow::anyhow!("could not resolve {file} path (no config dir)"))?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let listener = TcpListener::bind(loopback(0))
            .map_err(|e| anyhow::anyhow!("bind 127.0.0.1:0 failed: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| anyhow::anyhow!("could not read bound port: {e}"))?
            .port();
        let contents = format!("{port}\n{}", encode_token(&token));
        std::fs::write(&path, contents)
            .map_err(|e| anyhow::anyhow!("could not write port file {}: {e}", path.display()))?;
        Ok((listener, token))
    }

    pub fn connect_endpoint(file: &str) -> io::Result<Stream> {
        let (port, token) = read_port_file_named(file)
            .filter(|(p, _)| *p != 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no {file} file")))?;
        let mut stream = TcpStream::connect(loopback(port))?;
        tune(&stream);
        stream.write_all(&token)?;
        Ok(stream)
    }

    pub fn remove_endpoint(file: &str) {
        if let Some(path) = port_path_named(file) {
            let _ = std::fs::remove_file(path);
        }
    }

    pub fn endpoint_display() -> String {
        endpoint_display_named(PANE_PORT_FILE)
    }

    pub fn endpoint_display_named(file: &str) -> String {
        match read_port_file_named(file) {
            Some((port, _)) => format!("127.0.0.1:{port}"),
            None => "127.0.0.1:<unbound>".to_string(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::{Duration, Instant};

        const CLIENT_WITHIN: Duration = Duration::from_secs(10);

        fn accept_within(listener: &TcpListener) -> TcpStream {
            listener
                .set_nonblocking(true)
                .expect("put the listener in polling mode");
            let deadline = Instant::now() + CLIENT_WITHIN;
            let accepted = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "no client connected within {CLIENT_WITHIN:?}; the \
                             client thread most likely panicked, and without this \
                             deadline this test would hang until CI's job limit"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("accept failed: {e}"),
                }
            };
            listener
                .set_nonblocking(false)
                .expect("restore the listener to blocking");
            accepted
                .set_nonblocking(false)
                .expect("the accepted socket must block");
            accepted
                .set_read_timeout(Some(CLIENT_WITHIN))
                .expect("bound the handshake read too");
            accepted
        }

        #[test]
        fn token_hex_round_trips() {
            let token = make_token();
            assert_eq!(decode_token(&encode_token(&token)), Some(token));
        }

        #[test]
        fn decode_token_rejects_malformed() {
            assert!(decode_token("").is_none());
            assert!(decode_token("zz").is_none());
            assert!(decode_token(&"a".repeat(63)).is_none());
            assert!(decode_token(&"a".repeat(66)).is_none());
            assert!(decode_token(&"g".repeat(64)).is_none());
            assert!(decode_token(&"ab".repeat(32)).is_some());
        }

        #[test]
        fn parse_port_file_recovers_port_and_token() {
            let token = make_token();
            let contents = format!("54321\n{}", encode_token(&token));
            assert_eq!(parse_port_file(&contents), Some((54321, token)));
        }

        #[test]
        fn parse_port_file_rejects_missing_token() {
            assert!(parse_port_file("54321").is_none());
            assert!(parse_port_file("54321\n").is_none());
            assert!(parse_port_file("").is_none());
            assert!(parse_port_file("notaport\ndeadbeef").is_none());
        }

        #[test]
        fn tokens_match_is_exact() {
            let a = make_token();
            let mut b = a;
            assert!(tokens_match(&a, &b));
            b[TOKEN_LEN - 1] ^= 1;
            assert!(!tokens_match(&a, &b));
        }

        #[test]
        fn authenticate_with_accepts_only_the_matching_token() {
            let token = make_token();

            let mut good = std::io::Cursor::new(token.to_vec());
            assert!(authenticate_with(&mut good, &token).is_ok());

            let mut wrong_bytes = token;
            wrong_bytes[0] ^= 0xff;
            let mut wrong = std::io::Cursor::new(wrong_bytes.to_vec());
            let err = authenticate_with(&mut wrong, &token).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

            let mut short = std::io::Cursor::new(vec![0u8; TOKEN_LEN - 1]);
            assert!(authenticate_with(&mut short, &token).is_err());
        }

        #[test]
        fn loopback_handshake_authenticates_real_connection() {
            let token = make_token();
            let listener = TcpListener::bind(loopback(0)).expect("bind loopback");
            let port = listener.local_addr().unwrap().port();

            let good = std::thread::spawn(move || {
                let mut s = TcpStream::connect(loopback(port)).unwrap();
                s.write_all(&token).unwrap();
                s
            });
            let mut server_side = accept_within(&listener);
            assert!(authenticate_with(&mut server_side, &token).is_ok());
            let _keep = good.join().unwrap();

            let mut bad_token = token;
            bad_token[5] ^= 0xff;
            let bad = std::thread::spawn(move || {
                let mut s = TcpStream::connect(loopback(port)).unwrap();
                let _ = s.write_all(&bad_token);
            });
            let mut server_side2 = accept_within(&listener);
            assert!(authenticate_with(&mut server_side2, &token).is_err());
            bad.join().unwrap();
        }

        #[test]
        fn a_second_endpoint_gets_its_own_port_and_token() {
            const CONTROL: &str = "control.port";

            let dir = std::env::temp_dir().join(format!("tty7-wintok-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok();
            config::set_config_dir(dir);
            remove_endpoint(CONTROL);

            let (listener, token) = bind_endpoint(CONTROL).expect("bind the second endpoint");
            let port = listener.local_addr().unwrap().port();
            let recorded =
                std::fs::read_to_string(port_path_named(CONTROL).unwrap()).expect("marker file");
            let (file_port, file_token) = parse_port_file(&recorded).expect("marker file parses");
            assert_eq!(file_port, port, "the file records the port actually bound");
            assert_eq!(
                file_token, token,
                "and the token the listener will check for"
            );

            let good = std::thread::spawn(move || connect_endpoint(CONTROL).unwrap());
            let mut server_side = accept_within(&listener);
            assert!(check_endpoint_token(&mut server_side, &token).is_ok());
            let _keep = good.join().unwrap();

            let mut foreign = token;
            foreign[0] ^= 0xff;
            let bad = std::thread::spawn(move || {
                let mut s = TcpStream::connect(loopback(port)).unwrap();
                let _ = s.write_all(&foreign);
            });
            let mut server_side = accept_within(&listener);
            assert_eq!(
                check_endpoint_token(&mut server_side, &token)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            bad.join().unwrap();

            remove_endpoint(CONTROL);
        }

        #[test]
        fn bind_seeds_token_and_public_authenticate_accepts_a_file_token_client() {
            let dir = std::env::temp_dir().join(format!("tty7-wintok-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok();
            config::set_config_dir(dir);
            remove_stale_endpoint();

            let listener = bind().expect("bind under temp config dir");
            let bound_port = listener.local_addr().unwrap().port();

            let contents = std::fs::read_to_string(port_path().unwrap()).unwrap();
            let (port, token) = parse_port_file(&contents).expect("port file parses");
            assert_eq!(port, bound_port, "file records the actually-bound port");

            let good = std::thread::spawn(move || {
                let mut s = TcpStream::connect(loopback(port)).unwrap();
                s.write_all(&token).unwrap();
                s
            });
            let mut server_side = accept_within(&listener);
            assert!(authenticate(&mut server_side).is_ok());
            let _keep = good.join().unwrap();

            remove_stale_endpoint();
        }
    }
}
