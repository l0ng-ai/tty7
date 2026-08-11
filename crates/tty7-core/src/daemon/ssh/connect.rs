use std::borrow::Cow;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

use crate::daemon::protocol::{NativeSshSpec, SshAlgorithms, SshProxy};

use super::known_hosts;
use super::session::SshConnection;

pub enum Transport {
    Tcp(TcpStream),
    Process(ProcessStream),
    Channel(russh::ChannelStream<russh::client::Msg>),
}

impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            Transport::Process(s) => Pin::new(s).poll_read(cx, buf),
            Transport::Channel(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Transport::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            Transport::Process(s) => Pin::new(s).poll_write(cx, buf),
            Transport::Channel(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Tcp(s) => Pin::new(s).poll_flush(cx),
            Transport::Process(s) => Pin::new(s).poll_flush(cx),
            Transport::Channel(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            Transport::Process(s) => Pin::new(s).poll_shutdown(cx),
            Transport::Channel(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

pub struct ProcessStream {
    _child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: tokio::process::ChildStdout,
}

impl ProcessStream {
    pub fn from_parts(
        child: tokio::process::Child,
        stdin: tokio::process::ChildStdin,
        stdout: tokio::process::ChildStdout,
    ) -> ProcessStream {
        ProcessStream {
            _child: child,
            stdin: Some(stdin),
            stdout,
        }
    }

    fn stdin_mut(&mut self) -> std::io::Result<&mut tokio::process::ChildStdin> {
        self.stdin.as_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the process stream's write half is already closed",
            )
        })
    }
}

impl AsyncRead for ProcessStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for ProcessStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut().stdin_mut() {
            Ok(stdin) => Pin::new(stdin).poll_write(cx, buf),
            Err(e) => Poll::Ready(Err(e)),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut().stdin_mut() {
            Ok(stdin) => Pin::new(stdin).poll_flush(cx),
            Err(_) => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let Some(stdin) = this.stdin.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        match Pin::new(stdin).poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                this.stdin = None;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                this.stdin = None;
                Poll::Ready(Err(e))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub async fn build_transport(
    spec: &NativeSshSpec,
    jump: Option<Arc<SshConnection>>,
) -> anyhow::Result<Transport> {
    if let SshProxy::Command(template) = &spec.proxy {
        return spawn_proxy_command(template, &spec.host, spec.port, &spec.user);
    }
    if let Some(jump) = jump {
        let channel = jump
            .open_direct_tcpip(&spec.host, spec.port)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "jump host direct-tcpip to {}:{} failed: {e}",
                    spec.host,
                    spec.port
                )
            })?;
        return Ok(Transport::Channel(channel.into_stream()));
    }
    match &spec.proxy {
        SshProxy::Socks { host, port } => {
            let stream = socks5_connect(host, *port, &spec.host, spec.port).await?;
            Ok(Transport::Tcp(stream))
        }
        SshProxy::Http { host, port } => {
            let stream = http_connect(host, *port, &spec.host, spec.port).await?;
            Ok(Transport::Tcp(stream))
        }
        _ => {
            let stream = TcpStream::connect((spec.host.as_str(), spec.port))
                .await
                .map_err(|e| {
                    anyhow::anyhow!("connect to {}:{} failed: {e}", spec.host, spec.port)
                })?;
            Ok(Transport::Tcp(stream))
        }
    }
}

fn spawn_proxy_command(
    template: &str,
    host: &str,
    port: u16,
    user: &str,
) -> anyhow::Result<Transport> {
    let argv = proxy_command_argv(template, host, port, user);
    let mut it = argv.into_iter();
    let program = it
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty ProxyCommand"))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(it)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    crate::core::proc::hide_console_tokio(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn ProxyCommand failed: {e}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("ProxyCommand stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("ProxyCommand stdout unavailable"))?;
    Ok(Transport::Process(ProcessStream::from_parts(
        child, stdin, stdout,
    )))
}

pub fn proxy_command_argv(template: &str, host: &str, port: u16, user: &str) -> Vec<String> {
    shell_split(template)
        .into_iter()
        .map(|tok| substitute_tokens(&tok, host, port, user))
        .collect()
}

fn substitute_tokens(tok: &str, host: &str, port: u16, user: &str) -> String {
    let mut out = String::with_capacity(tok.len());
    let mut chars = tok.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('h') => out.push_str(host),
                Some('p') => out.push_str(&port.to_string()),
                Some('r') => out.push_str(user),
                Some('%') => out.push('%'),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has_token = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_token = true;
            }
            '\\' if !in_single => {
                if let Some(&next) = chars.peek() {
                    chars.next();
                    cur.push(next);
                    has_token = true;
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
}

async fn socks5_connect(
    proxy_host: &str,
    proxy_port: u16,
    target: &str,
    target_port: u16,
) -> anyhow::Result<TcpStream> {
    let mut s = TcpStream::connect((proxy_host, proxy_port))
        .await
        .map_err(|e| {
            anyhow::anyhow!("connect to SOCKS proxy {proxy_host}:{proxy_port} failed: {e}")
        })?;
    s.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut reply = [0u8; 2];
    s.read_exact(&mut reply).await?;
    if reply[0] != 0x05 || reply[1] != 0x00 {
        anyhow::bail!("SOCKS5 proxy refused no-auth (got {reply:?})");
    }
    let host_bytes = target.as_bytes();
    if host_bytes.len() > 255 {
        anyhow::bail!("SOCKS5 target host too long");
    }
    let mut req = vec![0x05, 0x01, 0x00, 0x03, host_bytes.len() as u8];
    req.extend_from_slice(host_bytes);
    req.extend_from_slice(&target_port.to_be_bytes());
    s.write_all(&req).await?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        anyhow::bail!("SOCKS5 CONNECT failed (reply code {})", head[1]);
    }
    let addr_len = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l).await?;
            l[0] as usize
        }
        other => anyhow::bail!("SOCKS5 unexpected bound ATYP {other}"),
    };
    let mut discard = vec![0u8; addr_len + 2];
    s.read_exact(&mut discard).await?;
    Ok(s)
}

async fn http_connect(
    proxy_host: &str,
    proxy_port: u16,
    target: &str,
    target_port: u16,
) -> anyhow::Result<TcpStream> {
    let mut s = TcpStream::connect((proxy_host, proxy_port))
        .await
        .map_err(|e| {
            anyhow::anyhow!("connect to HTTP proxy {proxy_host}:{proxy_port} failed: {e}")
        })?;
    let req = format!(
        "CONNECT {target}:{target_port} HTTP/1.1\r\nHost: {target}:{target_port}\r\nProxy-Connection: keep-alive\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await?;
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        s.read_exact(&mut byte).await?;
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            anyhow::bail!("HTTP CONNECT response headers too large");
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let status_ok = head
        .lines()
        .next()
        .map(|line| line.contains(" 200"))
        .unwrap_or(false);
    if !status_ok {
        let first = head.lines().next().unwrap_or("").trim();
        anyhow::bail!("HTTP CONNECT failed: {first}");
    }
    Ok(s)
}

pub fn build_config(spec: &NativeSshSpec) -> Arc<russh::client::Config> {
    // Every hop comes through here — the target and each jump host build their
    // own config from their own spec — so each asks known_hosts about itself.
    let known = known_hosts::known_algorithms(&spec.host, spec.port);
    let mut cfg = russh::client::Config {
        preferred: build_preferred(&spec.algorithms, &known),
        ..Default::default()
    };
    if let Some(iv) = spec.keepalive_interval_s.filter(|v| *v > 0) {
        cfg.keepalive_interval = Some(Duration::from_secs(u64::from(iv)));
    }
    if let Some(max) = spec.keepalive_count_max {
        cfg.keepalive_max = max as usize;
    }
    Arc::new(cfg)
}

fn build_preferred(
    a: &SshAlgorithms,
    known_host_keys: &[russh::keys::Algorithm],
) -> russh::Preferred {
    let mut p = russh::Preferred::DEFAULT;
    if !a.kex.is_empty() {
        let v: Vec<russh::kex::Name> = a
            .kex
            .iter()
            .filter_map(|s| russh::kex::Name::try_from(s.as_str()).ok())
            .collect();
        if !v.is_empty() {
            p.kex = Cow::Owned(v);
        }
    }
    if !a.cipher.is_empty() {
        let v: Vec<russh::cipher::Name> = a
            .cipher
            .iter()
            .filter_map(|s| russh::cipher::Name::try_from(s.as_str()).ok())
            .collect();
        if !v.is_empty() {
            p.cipher = Cow::Owned(v);
        }
    }
    if !a.mac.is_empty() {
        let v: Vec<russh::mac::Name> = a
            .mac
            .iter()
            .filter_map(|s| russh::mac::Name::try_from(s.as_str()).ok())
            .collect();
        if !v.is_empty() {
            p.mac = Cow::Owned(v);
        }
    }
    if !a.host_key.is_empty() {
        let v: Vec<russh::keys::Algorithm> = a
            .host_key
            .iter()
            .filter_map(|s| s.parse::<russh::keys::Algorithm>().ok())
            .collect();
        if !v.is_empty() {
            p.key = Cow::Owned(v);
        }
    } else if !known_host_keys.is_empty() {
        // What OpenSSH's `order_hostkeyalgs()` does: offer the algorithms this
        // host already has entries for ahead of the rest, so a server that has
        // both an ssh-rsa key on file and an ed25519 key on offer answers with
        // the one the user has already accepted. Without it the default order
        // picks ed25519, and a host known only by ssh-rsa raises a
        // key-you-have-never-seen prompt on every single connection.
        //
        // Reordering only, never filtering — a host whose keys have genuinely
        // rotated must still be able to negotiate — and skipped entirely when
        // the user pinned `HostKeyAlgorithms`, which OpenSSH also treats as the
        // last word.
        let mut ordered: Vec<russh::keys::Algorithm> = p.key.iter().cloned().collect();
        ordered.sort_by_key(|alg| !known_host_keys.iter().any(|k| same_key_type(k, alg)));
        p.key = Cow::Owned(ordered);
    }
    if !a.compression.is_empty() {
        let v: Vec<russh::compression::Name> = a
            .compression
            .iter()
            .filter_map(|s| russh::compression::Name::try_from(s.as_str()).ok())
            .collect();
        if !v.is_empty() {
            p.compression = Cow::Owned(v);
        }
    }
    p
}

/// Whether two host-key algorithms stand for the same *key*.
///
/// An `ssh-rsa` line in known_hosts parses to `Rsa { hash: None }`, but what
/// gets negotiated is one of `rsa-sha2-512`, `rsa-sha2-256` or `ssh-rsa` — three
/// signature algorithms over one key. Comparing with `==` would float only the
/// SHA-1 spelling to the front of the list, which is a downgrade dressed up as a
/// preference; OpenSSH's `order_hostkeyalgs()` matches on the key type too.
fn same_key_type(a: &russh::keys::Algorithm, b: &russh::keys::Algorithm) -> bool {
    use russh::keys::Algorithm;
    match (a, b) {
        (Algorithm::Rsa { .. }, Algorithm::Rsa { .. }) => true,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_command_substitutes_h_p_r_tokens() {
        let argv = proxy_command_argv("corkscrew proxy 8080 %h %p", "example.com", 2222, "deploy");
        assert_eq!(
            argv,
            vec!["corkscrew", "proxy", "8080", "example.com", "2222"]
        );
    }

    #[test]
    fn proxy_command_handles_quotes_and_percent_and_user() {
        let argv = proxy_command_argv(
            "sh -c 'nc -X connect -x %h:%p %r@host 100%%'",
            "h.example",
            22,
            "root",
        );
        assert_eq!(
            argv,
            vec!["sh", "-c", "nc -X connect -x h.example:22 root@host 100%",]
        );
    }

    #[test]
    fn shell_split_respects_double_quotes_and_escapes() {
        assert_eq!(shell_split(r#"a "b c" d\ e"#), vec!["a", "b c", "d e"]);
        assert_eq!(shell_split("   "), Vec::<String>::new());
    }

    #[test]
    fn build_preferred_keeps_defaults_for_empty_lists() {
        let a = SshAlgorithms::default();
        let p = build_preferred(&a, &[]);
        assert_eq!(p.kex, russh::Preferred::DEFAULT.kex);
        assert_eq!(p.cipher, russh::Preferred::DEFAULT.cipher);
        assert_eq!(p.key, russh::Preferred::DEFAULT.key);
    }

    #[test]
    fn build_preferred_drops_unknown_and_applies_known_entries() {
        let a = SshAlgorithms {
            cipher: vec!["totally-not-a-cipher".into(), "aes256-ctr".into()],
            ..Default::default()
        };
        let p = build_preferred(&a, &[]);
        let aes = russh::cipher::Name::try_from("aes256-ctr").unwrap();
        assert_eq!(p.cipher.as_ref(), &[aes]);
    }

    /// The default order leads with ed25519, so a host known only by an
    /// ssh-rsa key was greeted with a key it had never seen on every connect.
    #[test]
    fn build_preferred_floats_known_hosts_algorithms_to_the_front() {
        let rsa = russh::keys::Algorithm::Rsa { hash: None };
        let p = build_preferred(&SshAlgorithms::default(), &[rsa]);
        // All three RSA spellings sign for the one key on file, so all three
        // lead — anything less would pin the host to SHA-1 signatures.
        assert!(
            p.key
                .iter()
                .take(3)
                .all(|a| matches!(a, russh::keys::Algorithm::Rsa { .. })),
            "expected the RSA spellings first, got {:?}",
            p.key
        );
    }

    #[test]
    fn build_preferred_reordering_never_drops_an_algorithm() {
        let rsa = russh::keys::Algorithm::Rsa { hash: None };
        let p = build_preferred(&SshAlgorithms::default(), &[rsa]);
        let mut got: Vec<String> = p.key.iter().map(|a| a.to_string()).collect();
        let mut want: Vec<String> = russh::Preferred::DEFAULT
            .key
            .iter()
            .map(|a| a.to_string())
            .collect();
        got.sort();
        want.sort();
        assert_eq!(got, want);
    }

    /// OpenSSH stops reordering once `HostKeyAlgorithms` is explicit, because
    /// the user has already said what order they want.
    #[test]
    fn build_preferred_leaves_a_pinned_host_key_list_alone() {
        let a = SshAlgorithms {
            host_key: vec!["ssh-ed25519".into(), "rsa-sha2-512".into()],
            ..Default::default()
        };
        let p = build_preferred(&a, &[russh::keys::Algorithm::Rsa { hash: None }]);
        assert_eq!(
            p.key.as_ref(),
            &[
                russh::keys::Algorithm::Ed25519,
                russh::keys::Algorithm::Rsa {
                    hash: Some(russh::keys::HashAlg::Sha512)
                },
            ]
        );
    }
}
