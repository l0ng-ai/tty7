//! Port forwarding for native-SSH panes (Workstream 4).
//!
//! Three forward types ride the pane's shared [`SshConnection`] (russh channels,
//! never a control socket — every forward is native):
//!
//! - **Local** (FR-F1): a TCP listener on `bind_host:bind_port`; each accepted
//!   connection opens a `direct-tcpip` channel to `target_host:target_port` on the
//!   connection and [`bridge`]s the two with exact EOF/close propagation.
//! - **Dynamic / SOCKS5** (FR-F1): a local listener speaking a minimal, hand-rolled
//!   SOCKS5 (no-auth greeting, CONNECT for IPv4/IPv6/domain; BIND/UDP rejected).
//!   Each request opens a `direct-tcpip` to the negotiated target and bridges.
//! - **Remote** (FR-F1): a `tcpip-forward` global request on the connection;
//!   incoming `forwarded-tcpip` channels (via the [`super::handler::ClientHandler`])
//!   are matched against [`RemoteForwardTable`] and bridged to a fresh local TCP
//!   connection to the registered target. Unmatched channels are rejected.
//!
//! **Registry keying & blast radius.** [`SshForwardRegistry`] keys active forwards
//! by `pane_id` (so the UI lists them per pane) but each forward task holds an
//! `Arc<SshConnection>`, so a forward keeps the shared connection alive exactly
//! like `ssh -N`. When a pane dies the daemon calls
//! [`SshForwardRegistry::teardown_pane`], which aborts its listener tasks and
//! cancels its remote bindings; dropping the last `Arc` then tears the connection
//! down. When the *transport* drops, every pane sharing the connection dies as a
//! unit (FR-C2), so every forward attributed to those panes is torn down together.

use std::collections::HashMap;
use std::io;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::daemon::protocol::{
    ForwardStatus, LoopbackForward, ManagedForward, SshForwardKind, SshForwardRule,
};

use super::session::SshConnection;

/// Accept a connection, retrying transient errors instead of killing the
/// listener: ECONNABORTED (client gave up mid-handshake) and EMFILE/ENFILE
/// (fd pressure) are momentary, and exiting the accept loop on them would
/// leave the forward dead while its status still says "listening". `None`
/// only on errors that persist after a backoff (listener genuinely broken).
async fn accept_retrying(listener: &TcpListener) -> Option<(TcpStream, std::net::SocketAddr)> {
    let mut failures = 0u32;
    loop {
        match listener.accept().await {
            Ok(pair) => return Some(pair),
            Err(_) if failures >= 10 => return None,
            Err(_) => {
                failures += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50 << failures.min(5))).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bidirectional socket<->channel bridge (Tabby brief §5).
// ---------------------------------------------------------------------------

/// Bridge two duplex streams, propagating EOF and close in both directions: when
/// one side's read half hits EOF, the other side's write half is shut down (a
/// half-close), and once both directions have closed the bridge returns. This
/// mirrors Tabby's `setupSocketChannelEvents` (channel.eof→socket.end,
/// socket.end→channel.eof, close→destroy) so neither a socket nor a russh channel
/// is left half-open.
pub(super) async fn bridge<A, B>(a: A, b: B) -> io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);

    let a_to_b = async {
        tokio::io::copy(&mut ar, &mut bw).await?;
        // Source EOF'd: signal it downstream so the peer sees a clean close
        // rather than a stall.
        bw.shutdown().await
    };
    let b_to_a = async {
        tokio::io::copy(&mut br, &mut aw).await?;
        aw.shutdown().await
    };

    // Run both directions until each has hit EOF (or one errors). `try_join`
    // surfaces the first error and drops the other future, which closes its
    // half — the connection cannot be left half-open.
    tokio::try_join!(a_to_b, b_to_a)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal SOCKS5 (RFC 1928) for Dynamic forwards.
// ---------------------------------------------------------------------------

/// Negotiate a SOCKS5 CONNECT request on `s`: read the (no-auth) greeting, reply
/// with the no-auth method, read the CONNECT request, and return the requested
/// `(host, port)`. Rejects SOCKS4 (version byte `0x04`), any command other than
/// CONNECT (so BIND/UDP-ASSOCIATE are refused), and unknown address types. The
/// caller opens the upstream channel and then writes the final reply with
/// [`socks5_reply`].
pub(super) async fn socks5_negotiate<S>(s: &mut S) -> io::Result<(String, u16)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Greeting: VER, NMETHODS, METHODS...
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        // A SOCKS4 client sends 0x04 here; anything but 0x05 is unsupported.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported SOCKS version (only SOCKS5 is accepted)",
        ));
    }
    let nmethods = head[1] as usize;
    let mut methods = vec![0u8; nmethods];
    s.read_exact(&mut methods).await?;
    if !methods.contains(&0x00) {
        // No acceptable methods (0xFF) — we only implement no-auth.
        let _ = s.write_all(&[0x05, 0xFF]).await;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SOCKS5 client offered no no-auth method",
        ));
    }
    s.write_all(&[0x05, 0x00]).await?;

    // Request: VER, CMD, RSV, ATYP, ADDR, PORT.
    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    if req[0] != 0x05 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SOCKS5 request had wrong version",
        ));
    }
    if req[1] != 0x01 {
        // Only CONNECT (0x01); reject BIND (0x02) / UDP-ASSOCIATE (0x03).
        socks5_reply(s, 0x07).await?; // command not supported
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SOCKS5 command not supported (only CONNECT)",
        ));
    }
    let host = match req[3] {
        0x01 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            Ipv4Addr::from(a).to_string()
        }
        0x04 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            s.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "SOCKS5 domain not UTF-8")
            })?
        }
        other => {
            socks5_reply(s, 0x08).await?; // address type not supported
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("SOCKS5 unsupported address type {other}"),
            ));
        }
    };
    let mut port = [0u8; 2];
    s.read_exact(&mut port).await?;
    Ok((host, u16::from_be_bytes(port)))
}

/// Write a SOCKS5 reply with reply code `rep` (0x00 = success), a fixed
/// `0.0.0.0:0` bound address (clients ignore it for CONNECT).
pub(super) async fn socks5_reply<S>(s: &mut S, rep: u8) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    s.write_all(&[0x05, rep, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await
}

// ---------------------------------------------------------------------------
// Remote-forward table (consulted by the connection's Handler).
// ---------------------------------------------------------------------------

/// The set of `tcpip-forward` bindings registered on one connection, mapping a
/// remote bind address/port to the local target to connect incoming
/// `forwarded-tcpip` channels to. Shared (cheaply cloned `Arc`) between the
/// [`SshConnection`] and its [`super::handler::ClientHandler`]; a reused
/// connection keeps its bindings across panes.
#[derive(Clone, Default)]
pub struct RemoteForwardTable {
    inner: Arc<Mutex<HashMap<(String, u16), (String, u16)>>>,
}

impl RemoteForwardTable {
    /// Register a binding, refusing a duplicate: overwriting would hijack the
    /// existing forward's routing, and the caller's on-failure rollback would
    /// then delete the *original* entry, leaving its live server binding
    /// unroutable. Returns whether the key was free.
    pub(super) fn register(
        &self,
        bind_host: &str,
        bind_port: u16,
        target_host: &str,
        target_port: u16,
    ) -> bool {
        match self
            .inner
            .lock()
            .unwrap()
            .entry((bind_host.to_string(), bind_port))
        {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((target_host.to_string(), target_port));
                true
            }
        }
    }

    pub(super) fn unregister(&self, bind_host: &str, bind_port: u16) {
        self.inner
            .lock()
            .unwrap()
            .remove(&(bind_host.to_string(), bind_port));
    }

    /// Move a binding to a new (server-assigned) port when the client requested
    /// port 0.
    pub(super) fn rekey(&self, bind_host: &str, from_port: u16, to_port: u16) {
        let mut map = self.inner.lock().unwrap();
        if let Some(target) = map.remove(&(bind_host.to_string(), from_port)) {
            map.insert((bind_host.to_string(), to_port), target);
        }
    }

    /// Resolve an incoming `forwarded-tcpip` channel's connected address/port to a
    /// local target. Tries the exact `(address, port)` first, then a port-only
    /// match (the server may report `127.0.0.1` for a `localhost` bind, or
    /// `0.0.0.0` for an empty bind address) — but only when the port match is
    /// unambiguous: with two bindings on the same port and different addresses,
    /// guessing could bridge traffic to the wrong local target.
    pub(super) fn lookup(
        &self,
        connected_address: &str,
        connected_port: u16,
    ) -> Option<(String, u16)> {
        let map = self.inner.lock().unwrap();
        if let Some(t) = map.get(&(connected_address.to_string(), connected_port)) {
            return Some(t.clone());
        }
        let mut same_port = map.iter().filter(|((_, p), _)| *p == connected_port);
        match (same_port.next(), same_port.next()) {
            (Some((_, t)), None) => Some(t.clone()),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Managed-forward registry.
// ---------------------------------------------------------------------------

/// A live forward's teardown handle.
enum ForwardCancel {
    /// A Local/Dynamic accept loop. Held as the full `JoinHandle` (not just an
    /// `AbortHandle`) so [`SshForwardRegistry::cancel_entry`] can `abort()` *and*
    /// `await` it: `abort()` only *requests* cancellation, so awaiting is what
    /// guarantees the task — and the `TcpListener` it owns — is fully dropped,
    /// freeing the bound port before `remove`/`teardown_pane` returns.
    Task(JoinHandle<()>),
    /// A Remote binding to cancel via `cancel_tcpip_forward` on teardown.
    Remote {
        conn: Weak<SshConnection>,
        bind_host: String,
        bind_port: u16,
    },
    /// The forward never came up (bind/request failed); nothing to cancel.
    None,
}

struct ForwardEntry {
    id: u64,
    kind: SshForwardKind,
    bind_host: String,
    bind_port: u16,
    target_host: String,
    target_port: u16,
    description: Option<String>,
    status: ForwardStatus,
    cancel: ForwardCancel,
    /// True for a forward auto-created by a Cmd-clicked `localhost:PORT` link
    /// (FR-F4). Such entries are eligible for reuse when the same target is
    /// clicked again, and read as a plain Local row in the unified forwards list.
    auto_local: bool,
}

impl ForwardEntry {
    fn to_managed(&self, pane_id: u64) -> ManagedForward {
        ManagedForward {
            id: self.id,
            pane_id,
            kind: self.kind,
            bind_host: self.bind_host.clone(),
            bind_port: self.bind_port,
            target_host: self.target_host.clone(),
            target_port: self.target_port,
            description: self.description.clone(),
            status: self.status.clone(),
        }
    }
}

/// The per-process registry of managed forwards, owned by [`super::SshManager`].
#[derive(Default)]
pub struct SshForwardRegistry {
    panes: Mutex<HashMap<u64, Vec<ForwardEntry>>>,
    next_id: AtomicU64,
}

impl SshForwardRegistry {
    /// Establish a managed forward for `rule` on `conn`, attribute it to `pane_id`,
    /// and return the resulting [`ManagedForward`] (with a resolved bind port and a
    /// live status). Failures are reported as `ForwardStatus::Error`, never a hard
    /// error — a preconfigured forward that fails must not kill the session.
    pub async fn establish(
        &self,
        pane_id: u64,
        conn: Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> ManagedForward {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (bind_port, status, cancel) = match rule.kind {
            SshForwardKind::Local => self.start_local(&conn, rule).await,
            SshForwardKind::Dynamic => self.start_dynamic(&conn, rule).await,
            SshForwardKind::Remote => self.start_remote(&conn, rule).await,
        };
        let entry = ForwardEntry {
            id,
            kind: rule.kind,
            bind_host: rule.bind_host.clone(),
            bind_port,
            target_host: rule.target_host.clone(),
            target_port: rule.target_port,
            description: rule.description.clone(),
            status,
            cancel,
            auto_local: false,
        };
        let managed = entry.to_managed(pane_id);
        self.panes
            .lock()
            .unwrap()
            .entry(pane_id)
            .or_default()
            .push(entry);
        managed
    }

    /// The managed forwards attributed to `pane_id`, sorted by id (creation order).
    pub fn list(&self, pane_id: u64) -> Vec<ManagedForward> {
        let panes = self.panes.lock().unwrap();
        let mut list: Vec<_> = panes
            .get(&pane_id)
            .into_iter()
            .flatten()
            .map(|e| e.to_managed(pane_id))
            .collect();
        list.sort_by_key(|m| m.id);
        list
    }

    /// Remove one managed forward by id from `pane_id`, tearing down its listener
    /// or remote binding. Returns the pane's remaining forwards.
    pub async fn remove(&self, pane_id: u64, forward_id: u64) -> Vec<ManagedForward> {
        let removed = {
            let mut panes = self.panes.lock().unwrap();
            if let Some(entries) = panes.get_mut(&pane_id) {
                if let Some(pos) = entries.iter().position(|e| e.id == forward_id) {
                    Some(entries.remove(pos))
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(entry) = removed {
            Self::cancel_entry(entry).await;
        }
        self.list(pane_id)
    }

    /// Tear down every forward attributed to `pane_id` (called when the pane dies —
    /// on explicit kill, reclaim, or connection loss). Local/Dynamic listeners are
    /// aborted synchronously; remote bindings are cancelled best-effort.
    pub async fn teardown_pane(&self, pane_id: u64) {
        let entries = self.panes.lock().unwrap().remove(&pane_id);
        for entry in entries.into_iter().flatten() {
            Self::cancel_entry(entry).await;
        }
    }

    async fn cancel_entry(entry: ForwardEntry) {
        match entry.cancel {
            ForwardCancel::Task(handle) => {
                // `abort()` only *schedules* cancellation; awaiting the handle
                // drives the task to completion so its `TcpListener` is dropped
                // (socket closed) before we return. The task was cancelled, so
                // the `JoinError` is expected and ignored.
                handle.abort();
                let _ = handle.await;
            }
            ForwardCancel::Remote {
                conn,
                bind_host,
                bind_port,
            } => {
                if let Some(conn) = conn.upgrade() {
                    conn.cancel_remote_forward(&bind_host, bind_port).await;
                }
            }
            ForwardCancel::None => {}
        }
    }

    async fn start_local(
        &self,
        conn: &Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> (u16, ForwardStatus, ForwardCancel) {
        let listener = match TcpListener::bind((rule.bind_host.as_str(), rule.bind_port)).await {
            Ok(l) => l,
            Err(e) => {
                return (
                    rule.bind_port,
                    ForwardStatus::Error(format!(
                        "bind {}:{} failed: {e}",
                        rule.bind_host, rule.bind_port
                    )),
                    ForwardCancel::None,
                );
            }
        };
        let bound = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(rule.bind_port);
        let conn = conn.clone();
        let target_host = rule.target_host.clone();
        let target_port = rule.target_port;
        let handle = tokio::spawn(async move {
            loop {
                let sock = match accept_retrying(&listener).await {
                    Some((sock, _peer)) => sock,
                    None => break,
                };
                if !conn.is_alive() {
                    break;
                }
                let conn = conn.clone();
                let target_host = target_host.clone();
                tokio::spawn(async move {
                    match conn.open_direct_tcpip(&target_host, target_port).await {
                        Ok(channel) => {
                            let _ = bridge(sock, channel.into_stream()).await;
                        }
                        // Remote refused (or the connection died): drop the client
                        // socket. No secrets in the log.
                        Err(e) => {
                            log::info!("local forward to {target_host}:{target_port} rejected: {e}")
                        }
                    }
                });
            }
        });
        (bound, ForwardStatus::Listening, ForwardCancel::Task(handle))
    }

    async fn start_dynamic(
        &self,
        conn: &Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> (u16, ForwardStatus, ForwardCancel) {
        let listener = match TcpListener::bind((rule.bind_host.as_str(), rule.bind_port)).await {
            Ok(l) => l,
            Err(e) => {
                return (
                    rule.bind_port,
                    ForwardStatus::Error(format!(
                        "bind {}:{} failed: {e}",
                        rule.bind_host, rule.bind_port
                    )),
                    ForwardCancel::None,
                );
            }
        };
        let bound = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(rule.bind_port);
        let conn = conn.clone();
        let handle = tokio::spawn(async move {
            loop {
                let sock = match accept_retrying(&listener).await {
                    Some((sock, _peer)) => sock,
                    None => break,
                };
                if !conn.is_alive() {
                    break;
                }
                let conn = conn.clone();
                tokio::spawn(async move {
                    let mut sock = sock;
                    let (host, port) = match socks5_negotiate(&mut sock).await {
                        Ok(t) => t,
                        Err(e) => {
                            log::info!("dynamic forward: SOCKS5 negotiation failed: {e}");
                            return;
                        }
                    };
                    match conn.open_direct_tcpip(&host, port).await {
                        Ok(channel) => {
                            if socks5_reply(&mut sock, 0x00).await.is_err() {
                                return;
                            }
                            let _ = bridge(sock, channel.into_stream()).await;
                        }
                        Err(e) => {
                            // 0x05 = connection refused by destination host.
                            let _ = socks5_reply(&mut sock, 0x05).await;
                            log::info!("dynamic forward to {host}:{port} rejected: {e}");
                        }
                    }
                });
            }
        });
        (bound, ForwardStatus::Listening, ForwardCancel::Task(handle))
    }

    async fn start_remote(
        &self,
        conn: &Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> (u16, ForwardStatus, ForwardCancel) {
        match conn
            .add_remote_forward(
                &rule.bind_host,
                rule.bind_port,
                &rule.target_host,
                rule.target_port,
            )
            .await
        {
            Ok(bound) => (
                bound,
                ForwardStatus::Listening,
                ForwardCancel::Remote {
                    conn: Arc::downgrade(conn),
                    bind_host: rule.bind_host.clone(),
                    bind_port: bound,
                },
            ),
            Err(e) => (
                rule.bind_port,
                ForwardStatus::Error(format!("remote forward request denied: {e}")),
                ForwardCancel::None,
            ),
        }
    }

    // ---- Native loopback (FR-F4) --------------------------------------------

    /// Ensure a native-SSH loopback forward `127.0.0.1:<ephemeral> → host:port`
    /// exists for `pane_id`, reusing an existing auto-created one for the same
    /// target. The forward is registered in the *same* managed registry as
    /// [`Self::establish`], so it shows up as a plain Local row in `list(pane_id)`
    /// — there is no separate loopback bookkeeping. Returns the `LoopbackForward`
    /// reply shape the GUI's Cmd-click flow consumes (just the local port), so the
    /// wire reply is unchanged.
    ///
    /// `_target` (the pane's remote hostname) is retained for call-site
    /// compatibility; dedup keys on the concrete `remote_host:remote_port`.
    pub async fn ensure_loopback(
        &self,
        pane_id: u64,
        conn: Arc<SshConnection>,
        _target: &str,
        remote_host: &str,
        remote_port: u16,
    ) -> io::Result<LoopbackForward> {
        // Dedup: a live auto-forward to the same target is reused rather than
        // duplicated (preserving the old `ensure_loopback` behavior).
        if let Some(local_port) = self.find_auto_local(pane_id, remote_host, remote_port) {
            return Ok(LoopbackForward { local_port });
        }
        let rule = SshForwardRule {
            kind: SshForwardKind::Local,
            bind_host: "127.0.0.1".to_string(),
            bind_port: 0,
            target_host: remote_host.to_string(),
            target_port: remote_port,
            description: Some(format!("localhost link → :{remote_port}")),
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (bind_port, status, cancel) = self.start_local(&conn, &rule).await;
        // A bind failure must surface to the Cmd-click caller (it previously
        // propagated via `?`), and no dead entry is registered.
        if let ForwardStatus::Error(e) = &status {
            return Err(io::Error::other(e.clone()));
        }
        let entry = ForwardEntry {
            id,
            kind: SshForwardKind::Local,
            bind_host: rule.bind_host.clone(),
            bind_port,
            target_host: rule.target_host.clone(),
            target_port: rule.target_port,
            description: rule.description.clone(),
            status,
            cancel,
            auto_local: true,
        };
        self.panes
            .lock()
            .unwrap()
            .entry(pane_id)
            .or_default()
            .push(entry);
        Ok(LoopbackForward {
            local_port: bind_port,
        })
    }

    /// The local port of a live auto-created loopback forward on `pane_id` targeting
    /// `remote_host:remote_port`, if one exists (dedup for Cmd-click).
    fn find_auto_local(&self, pane_id: u64, remote_host: &str, remote_port: u16) -> Option<u16> {
        let panes = self.panes.lock().unwrap();
        panes
            .get(&pane_id)?
            .iter()
            .find(|e| {
                e.auto_local
                    && e.kind == SshForwardKind::Local
                    && e.target_host == remote_host
                    && e.target_port == remote_port
                    && matches!(e.status, ForwardStatus::Listening)
            })
            .map(|e| e.bind_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A SOCKS4 client (version byte `0x04`) is rejected outright.
    #[tokio::test]
    async fn socks5_rejects_v4() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[0x04, 0x01]).await.unwrap();
        let err = socks5_negotiate(&mut server).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// A well-formed v5 CONNECT to an IPv4 address is parsed and the method reply is
    /// the no-auth selection.
    #[tokio::test]
    async fn socks5_v5_connect_ipv4() {
        let (mut client, mut server) = tokio::io::duplex(64);
        // Greeting (1 method: no-auth) + CONNECT to 1.2.3.4:80.
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        client
            .write_all(&[0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0x00, 0x50])
            .await
            .unwrap();
        let (host, port) = socks5_negotiate(&mut server).await.unwrap();
        assert_eq!(host, "1.2.3.4");
        assert_eq!(port, 80);
        // Method-selection reply is VER=5, METHOD=0 (no auth).
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00]);
    }

    /// A v5 CONNECT with a domain-name address (ATYP=3).
    #[tokio::test]
    async fn socks5_v5_connect_domain() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let host = b"example.com";
        let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
        req.extend_from_slice(host);
        req.extend_from_slice(&443u16.to_be_bytes());
        client.write_all(&req).await.unwrap();
        // Negotiate before draining the reply: on a single-threaded test runtime
        // the writer must run first, or the reply read would deadlock.
        let (host, port) = socks5_negotiate(&mut server).await.unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00]);
    }

    /// A v5 CONNECT with an IPv6 address (ATYP=4).
    #[tokio::test]
    async fn socks5_v5_connect_ipv6() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut req = vec![0x05, 0x01, 0x00, 0x04];
        req.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        req.extend_from_slice(&22u16.to_be_bytes());
        client.write_all(&req).await.unwrap();
        // Negotiate before draining the reply (see the domain test).
        let (host, port) = socks5_negotiate(&mut server).await.unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 22);
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00]);
    }

    /// A v5 BIND command (0x02) is rejected with a "command not supported" reply.
    #[tokio::test]
    async fn socks5_rejects_bind_command() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        client
            .write_all(&[0x05, 0x02, 0x00, 0x01, 1, 2, 3, 4, 0x00, 0x50])
            .await
            .unwrap();
        let err = socks5_negotiate(&mut server).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // Method reply then a 0x07 (command not supported) reply.
        let mut method = [0u8; 2];
        client.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [0x05, 0x00]);
        let mut rep = [0u8; 10];
        client.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[1], 0x07);
    }

    /// The bridge forwards bytes A→B and propagates the A-side EOF as a clean close
    /// on the B side (and streams a reply back B→A).
    #[tokio::test]
    async fn bridge_propagates_data_and_eof_both_directions() {
        // client_a <-> a  ...bridge...  b <-> server_b
        let (mut client_a, a) = tokio::io::duplex(64);
        let (b, mut server_b) = tokio::io::duplex(64);
        let bridged = tokio::spawn(async move { bridge(a, b).await });

        // A→B data, then close A's write half.
        client_a.write_all(b"ping").await.unwrap();
        client_a.shutdown().await.unwrap();

        let mut got = Vec::new();
        server_b.read_to_end(&mut got).await.unwrap();
        assert_eq!(
            got, b"ping",
            "A→B data delivered and A-side EOF closed B read"
        );

        // B→A reply after the far side EOF'd — must still flow, then close.
        server_b.write_all(b"pong").await.unwrap();
        server_b.shutdown().await.unwrap();
        let mut back = Vec::new();
        client_a.read_to_end(&mut back).await.unwrap();
        assert_eq!(
            back, b"pong",
            "B→A reply delivered and B-side EOF closed A read"
        );

        bridged.await.unwrap().unwrap();
    }

    /// The remote-forward table resolves exact matches and falls back to any binding
    /// on the same port (server may report a different bind address).
    #[test]
    fn remote_forward_table_lookup() {
        let table = RemoteForwardTable::default();
        table.register("localhost", 9000, "127.0.0.1", 3000);
        assert_eq!(
            table.lookup("localhost", 9000),
            Some(("127.0.0.1".to_string(), 3000))
        );
        // The server reported 127.0.0.1 for a localhost bind → port fallback.
        assert_eq!(
            table.lookup("127.0.0.1", 9000),
            Some(("127.0.0.1".to_string(), 3000))
        );
        assert_eq!(table.lookup("localhost", 9999), None);
        table.unregister("localhost", 9000);
        assert_eq!(table.lookup("localhost", 9000), None);
    }

    /// The registry's add/list/remove/teardown bookkeeping, independent of a live
    /// connection (entries are inserted directly, bypassing `establish` which needs
    /// an authenticated `SshConnection`). Aborting the cancel task on remove/teardown
    /// is what a real listener teardown does.
    #[tokio::test]
    async fn registry_add_list_remove_teardown_bookkeeping() {
        let reg = SshForwardRegistry::default();
        let make = |id: u64, port: u16| {
            let task = tokio::spawn(async { std::future::pending::<()>().await });
            ForwardEntry {
                id,
                kind: SshForwardKind::Local,
                bind_host: "127.0.0.1".into(),
                bind_port: port,
                target_host: "h".into(),
                target_port: 80,
                description: None,
                status: ForwardStatus::Listening,
                cancel: ForwardCancel::Task(task),
                auto_local: false,
            }
        };
        {
            let mut panes = reg.panes.lock().unwrap();
            let entries = panes.entry(7).or_default();
            entries.push(make(0, 8000));
            entries.push(make(1, 8001));
        }
        // list is per-pane and sorted by id.
        let list = reg.list(7);
        assert_eq!(list.iter().map(|m| m.id).collect::<Vec<_>>(), vec![0, 1]);
        assert!(reg.list(99).is_empty(), "other panes see nothing");

        // remove drops just the one forward and returns the remainder.
        let remaining = reg.remove(7, 0).await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, 1);

        // teardown clears the pane entirely (blast-radius on death).
        reg.teardown_pane(7).await;
        assert!(reg.list(7).is_empty());
    }

    /// Removing (or tearing down) a Local/Dynamic forward must fully drop its
    /// accept-loop task — and the `TcpListener` it owns — *before* the call
    /// returns, so the bound port is freed synchronously. A plain
    /// `AbortHandle::abort()` only *requests* cancellation, so the task (and its
    /// socket) can outlive the call and leak the port; `cancel_entry` must abort
    /// *and* await.
    ///
    /// The assertion is race-free: the accept task owns both a real bound
    /// `TcpListener` (fidelity with `start_local`) and a clone of an `Arc` guard.
    /// Once the task's future is dropped, the guard clone is dropped, so the
    /// registry-side `Arc` becomes uniquely owned. With only `abort()` (no await)
    /// the task has not been polled on this current-thread runtime when the call
    /// returns, so the guard is still held (`strong_count == 2`) — the bug.
    #[tokio::test]
    async fn remove_frees_listening_socket_synchronously() {
        async fn spawn_listener_entry(id: u64, guard: &Arc<()>) -> ForwardEntry {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let guard = guard.clone();
            // Mirror start_local's accept loop: the task owns the listener, so
            // only fully dropping the task closes the socket. `guard` rides along
            // and is dropped exactly when the task's future is dropped.
            let handle = tokio::spawn(async move {
                let _guard = guard;
                loop {
                    if listener.accept().await.is_err() {
                        break;
                    }
                }
            });
            ForwardEntry {
                id,
                kind: SshForwardKind::Local,
                bind_host: "127.0.0.1".into(),
                bind_port: port,
                target_host: "h".into(),
                target_port: 80,
                description: None,
                status: ForwardStatus::Listening,
                cancel: ForwardCancel::Task(handle),
                auto_local: false,
            }
        }

        let reg = SshForwardRegistry::default();

        // remove() path: the task's future (holding the listener) must be gone.
        let guard = Arc::new(());
        let entry = spawn_listener_entry(0, &guard).await;
        reg.panes.lock().unwrap().entry(1).or_default().push(entry);
        assert_eq!(
            Arc::strong_count(&guard),
            2,
            "task holds the socket while live"
        );
        reg.remove(1, 0).await;
        assert_eq!(
            Arc::strong_count(&guard),
            1,
            "remove() must drop the accept task (and its TcpListener) synchronously"
        );

        // teardown_pane() path (pane death / connection loss) frees it too.
        let guard2 = Arc::new(());
        let entry2 = spawn_listener_entry(1, &guard2).await;
        reg.panes.lock().unwrap().entry(2).or_default().push(entry2);
        reg.teardown_pane(2).await;
        assert_eq!(
            Arc::strong_count(&guard2),
            1,
            "teardown_pane() must drop every accept task synchronously"
        );
    }

    /// `rekey` moves a binding to the server-assigned port (bind_port 0 case).
    #[test]
    fn remote_forward_table_rekey() {
        let table = RemoteForwardTable::default();
        table.register("", 0, "127.0.0.1", 3000);
        table.rekey("", 0, 40000);
        assert_eq!(
            table.lookup("", 40000),
            Some(("127.0.0.1".to_string(), 3000))
        );
        assert_eq!(table.lookup("", 0), None);
    }
}
