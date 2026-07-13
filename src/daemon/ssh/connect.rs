//! Transport construction and russh `Config` for the native SSH engine.
//!
//! Every transport is reduced to a single [`Transport`] value implementing
//! `AsyncRead + AsyncWrite`, which `russh::client::connect_stream` accepts:
//!
//! - **Direct** — a plain `TcpStream`.
//! - **ProxyCommand** — spawn the command; its stdio is the transport. tty7
//!   substitutes `%h`/`%p`/`%r` itself (the gap Tabby left, PRD FR-C1 / #11058).
//! - **SOCKS5 / HTTP CONNECT** — a `TcpStream` to the proxy, handshaked up to the
//!   target (no-auth SOCKS5; bare HTTP `CONNECT`), then used directly.
//! - **Jump host** — a `direct-tcpip` channel opened on an already-authenticated
//!   jump [`SshConnection`], turned into a stream. Multi-level chains fall out of
//!   the manager establishing the jump connection recursively before calling here.

use std::borrow::Cow;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

use crate::daemon::protocol::{NativeSshSpec, SshAlgorithms, SshProxy};

use super::session::SshConnection;

/// A concrete transport stream for `connect_stream`. An enum (rather than a boxed
/// trait object) so each variant's `AsyncRead`/`AsyncWrite` is a direct delegate.
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

/// A spawned `ProxyCommand`'s stdio as one duplex stream. `kill_on_drop` reaps the
/// process when the transport is dropped.
pub struct ProcessStream {
    // Held so the child is reaped on drop; not otherwise read.
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
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
        Pin::new(&mut self.get_mut().stdin).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stdin).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stdin).poll_shutdown(cx)
    }
}

/// Build the transport for `spec`, given an already-established `jump` connection
/// when the spec chains through one. Precedence mirrors OpenSSH/Tabby:
/// ProxyCommand > jump host > SOCKS5 > HTTP > direct.
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
        // None (or Command, handled above): direct.
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
    Ok(Transport::Process(ProcessStream {
        _child: child,
        stdin,
        stdout,
    }))
}

/// Split a ProxyCommand template into argv and substitute the OpenSSH tokens
/// `%h` (host), `%p` (port), `%r` (remote user), and `%%` (a literal `%`). Public
/// for unit testing.
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
                // Unknown token: keep both characters verbatim.
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

/// A minimal POSIX-ish word splitter for ProxyCommand: honors single quotes,
/// double quotes, and backslash escaping; splits on unquoted whitespace.
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

/// SOCKS5 CONNECT (no authentication) to `target:target_port` via `proxy`.
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
    // Greeting: VER=5, one method, 0x00 = no auth.
    s.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut reply = [0u8; 2];
    s.read_exact(&mut reply).await?;
    if reply[0] != 0x05 || reply[1] != 0x00 {
        anyhow::bail!("SOCKS5 proxy refused no-auth (got {reply:?})");
    }
    // CONNECT request with a domain-name address (ATYP=3).
    let host_bytes = target.as_bytes();
    if host_bytes.len() > 255 {
        anyhow::bail!("SOCKS5 target host too long");
    }
    let mut req = vec![0x05, 0x01, 0x00, 0x03, host_bytes.len() as u8];
    req.extend_from_slice(host_bytes);
    req.extend_from_slice(&target_port.to_be_bytes());
    s.write_all(&req).await?;
    // Reply: VER, REP, RSV, ATYP, BND.ADDR, BND.PORT.
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
    let mut discard = vec![0u8; addr_len + 2]; // address + port
    s.read_exact(&mut discard).await?;
    Ok(s)
}

/// HTTP `CONNECT` tunnel to `target:target_port` via `proxy`.
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
    // Read until the end of headers (\r\n\r\n). Bounded so a hostile proxy can't
    // make us buffer without limit.
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

/// Build the russh client config from the spec: keepalive, and algorithm
/// preferences (empty list per family = russh's secure default for that family).
pub fn build_config(spec: &NativeSshSpec) -> Arc<russh::client::Config> {
    let mut cfg = russh::client::Config {
        preferred: build_preferred(&spec.algorithms),
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

/// Start from russh's default preference and override only the families the user
/// specified. Unparseable entries are dropped; if a user list parses to nothing,
/// that family keeps the default rather than becoming empty (which would offer no
/// algorithms and fail negotiation).
fn build_preferred(a: &SshAlgorithms) -> russh::Preferred {
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
        let p = build_preferred(&a);
        // Empty spec → unchanged russh default.
        assert_eq!(p.kex, russh::Preferred::DEFAULT.kex);
        assert_eq!(p.cipher, russh::Preferred::DEFAULT.cipher);
    }

    #[test]
    fn build_preferred_drops_unknown_and_applies_known_entries() {
        let a = SshAlgorithms {
            cipher: vec!["totally-not-a-cipher".into(), "aes256-ctr".into()],
            ..Default::default()
        };
        let p = build_preferred(&a);
        // The unknown entry is filtered; only the known one is applied.
        let aes = russh::cipher::Name::try_from("aes256-ctr").unwrap();
        assert_eq!(p.cipher.as_ref(), &[aes]);
    }
}
