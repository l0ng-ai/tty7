//! Native SSH session engine (Workstream 2).
//!
//! A single [`SshManager`] owns one tokio runtime and the registry of live
//! [`SshConnection`]s. The rest of the daemon is std-threads and never enters this
//! runtime; a native-SSH pane crosses the boundary only through the blocking
//! `Read`/`Write` adapters in [`session`] (fed by the async channel driver) and
//! the [`PromptBroker`] (auth/host-key round-trips). See `docs/ssh-native-architecture.md`.
//!
//! ## Connection reuse & the API WS4/WS5 build on (FR-C2)
//! Connections are keyed by [`ConnectionKey`] (host/port/user/proxy/jump chain).
//! A spawn for a key with a live connection reuses it — a new tab opens a fresh
//! *channel*, never a fresh authentication. Port-forwards (WS4) and SFTP (WS5)
//! reach a pane's connection through the same registry and open their own channels
//! on it: [`SshConnection::open_direct_tcpip`] (Local/Dynamic forwards, and the
//! jump transport) and [`SshConnection::open_session_channel`] (SFTP subsystem).
//! `DaemonPane::ssh_connection` (in `daemon::pane`) exposes a pane's connection.

pub mod broker;
pub mod forward;
pub mod known_hosts;
pub mod session;
pub mod sftp;

mod auth;
mod connect;
mod handler;

pub use broker::PromptBroker;
pub use forward::SshForwardRegistry;
pub use session::{ChannelCmd, SharedConnection, SshConnection, SshSessionHandle};

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use russh::Pty;

use crate::daemon::protocol::{
    LoopbackForward, LoopbackForwardId, LoopbackForwardInfo, ManagedForward, NativeSshSpec,
    SshForwardRule, SshPhase, WinSize,
};

use forward::RemoteForwardTable;
use handler::ClientHandler;
use session::drive_channel;

/// Default connect+auth budget when the spec doesn't set one.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Identifies a reusable connection: same key ⇒ same authenticated transport.
/// Includes the full proxy configuration and (recursively) the jump chain, so two
/// specs that differ only in how they *reach* the host don't collide.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ConnectionKey(String);

impl ConnectionKey {
    pub fn from_spec(spec: &NativeSshSpec) -> Self {
        use crate::daemon::protocol::SshProxy;
        let mut s = format!("{}@{}:{}", spec.user, spec.host, spec.port);
        match &spec.proxy {
            SshProxy::None => {}
            SshProxy::Command(c) => s.push_str(&format!("|cmd:{c}")),
            SshProxy::Socks { host, port } => s.push_str(&format!("|socks:{host}:{port}")),
            SshProxy::Http { host, port } => s.push_str(&format!("|http:{host}:{port}")),
        }
        if let Some(jump) = &spec.jump {
            s.push_str("|jump:");
            s.push_str(&ConnectionKey::from_spec(jump).0);
        }
        ConnectionKey(s)
    }
}

/// Per-key reuse slot: a `Weak` behind an async mutex, so establishing a new
/// connection for a key serializes (no duplicate connects) without serializing
/// *different* keys.
type ConnSlot = Arc<tokio::sync::Mutex<Weak<SshConnection>>>;

pub struct SshManager {
    runtime: tokio::runtime::Runtime,
    conns: Mutex<HashMap<ConnectionKey, ConnSlot>>,
    /// The WS4 managed-forward registry (Local/Remote/Dynamic + native loopback),
    /// driven on this manager's runtime.
    forwards: SshForwardRegistry,
}

impl SshManager {
    /// The process-wide engine. Built lazily on first native-SSH spawn.
    pub fn global() -> &'static SshManager {
        static MANAGER: OnceLock<SshManager> = OnceLock::new();
        MANAGER.get_or_init(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("tty7-ssh-rt")
                .build()
                .expect("build tty7 ssh runtime");
            SshManager {
                runtime,
                conns: Mutex::new(HashMap::new()),
                forwards: SshForwardRegistry::default(),
            }
        })
    }

    /// A handle to the engine's tokio runtime. The SFTP layer (`ssh::sftp`) uses
    /// it to `block_on` one-shot operations and `spawn` background transfer jobs
    /// from the daemon's std threads (the server connection threads) without owning
    /// a second runtime. Safe to call from a non-async thread; `block_on` on the
    /// returned handle drives the future on the caller and panics only if called
    /// from *within* a runtime worker (the server threads never are).
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    // ---- Synchronous forward API for the (std-thread) daemon server ----------
    //
    // The server dispatch runs on plain std threads; these block on the runtime
    // for the async establishment/teardown while returning results synchronously.

    /// Establish a managed forward on `conn` for `pane_id`; returns the pane's
    /// forwards after the add.
    pub fn add_forward(
        &self,
        pane_id: u64,
        conn: Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> Vec<ManagedForward> {
        self.runtime.block_on(async {
            self.forwards.establish(pane_id, conn, rule).await;
            self.forwards.list(pane_id)
        })
    }

    /// Remove a managed forward by id; returns the pane's remaining forwards.
    pub fn remove_forward(&self, pane_id: u64, forward_id: u64) -> Vec<ManagedForward> {
        self.runtime
            .block_on(self.forwards.remove(pane_id, forward_id))
    }

    /// List a pane's managed forwards.
    pub fn list_forwards(&self, pane_id: u64) -> Vec<ManagedForward> {
        self.forwards.list(pane_id)
    }

    /// Tear down every forward attributed to `pane_id` (pane death / blast radius).
    /// Detached on the runtime so a pane's `Drop` (which runs on a connection
    /// thread) never blocks on a remote `cancel_tcpip_forward` round-trip.
    pub fn teardown_pane_forwards(&'static self, pane_id: u64) {
        self.runtime.spawn(async move {
            self.forwards.teardown_pane(pane_id).await;
        });
    }

    /// Ensure a native-SSH loopback forward for a Cmd-clicked `localhost` URL (FR-F4).
    pub fn ensure_loopback_forward(
        &self,
        pane_id: u64,
        conn: Arc<SshConnection>,
        target: &str,
        remote_host: &str,
        remote_port: u16,
    ) -> std::io::Result<LoopbackForward> {
        self.runtime.block_on(self.forwards.ensure_loopback(
            pane_id,
            conn,
            target,
            remote_host,
            remote_port,
        ))
    }

    /// Loopback forwards are no longer tracked separately — a Cmd-clicked
    /// `localhost` link registers a plain Local managed forward (see
    /// [`SshForwardRegistry::ensure_loopback`]), surfaced through `list_forwards`.
    /// This wire endpoint is kept for protocol compatibility and always empty.
    pub fn list_loopback_forwards(&self) -> Vec<LoopbackForwardInfo> {
        Vec::new()
    }

    /// No-op: there is no separate loopback registry to close from (kept for
    /// protocol compatibility). Auto forwards are removed via the managed list.
    pub fn close_loopback_forward(&self, _id: &LoopbackForwardId) -> bool {
        false
    }

    /// Kick off a native-SSH shell for a pane. Returns immediately; the connect →
    /// auth → shell sequence runs on the runtime and drives the pane through the
    /// provided bridge ends. All progress/prompt frames go via `broker`.
    ///
    /// On any failure the task emits `SshStatus::Failed`, writes a one-line
    /// diagnostic into the output stream, and drops `data_tx` — which EOFs the
    /// pane's reader and surfaces as the usual `Exited`, so a failed connect looks
    /// to the rest of the daemon exactly like a shell that exited.
    pub fn spawn_native_session(
        &'static self,
        pane_id: u64,
        spec: Box<NativeSshSpec>,
        size: WinSize,
        broker: Arc<PromptBroker>,
        data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
        cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ChannelCmd>,
        conn_slot: SharedConnection,
    ) {
        self.runtime.spawn(async move {
            if let Err(reason) = self
                .run_session(
                    pane_id,
                    &spec,
                    size,
                    &broker,
                    data_tx.clone(),
                    cmd_rx,
                    &conn_slot,
                )
                .await
            {
                broker.status(SshPhase::Failed {
                    reason: reason.clone(),
                });
                // A visible, human-readable line so the pane isn't just a blank
                // that vanishes — even before WS3 renders SshStatus.
                let line = format!("\r\n\x1b[31mtty7: SSH connection failed: {reason}\x1b[0m\r\n");
                let _ = data_tx.send(line.into_bytes()).await;
                // Dropping data_tx (and cmd_rx already moved) EOFs the reader.
            }
        });
    }

    async fn run_session(
        &'static self,
        pane_id: u64,
        spec: &NativeSshSpec,
        size: WinSize,
        broker: &Arc<PromptBroker>,
        data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
        cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ChannelCmd>,
        conn_slot: &SharedConnection,
    ) -> Result<(), String> {
        broker.status(SshPhase::Connecting);

        // Note: the connect timeout is applied *inside* `open_connection`, around
        // the transport + SSH handshake only — never around interactive auth,
        // which the user may reasonably take a while to complete (the broker
        // enforces its own per-prompt timeout).
        let (mut conn, reused) = self
            .open_connection(spec, broker)
            .await
            .map_err(|e| format!("{e}"))?;

        // Publish the connection so the pane (and WS4/WS5) can open further
        // channels on it. A `Weak`, so this never keeps the connection alive past
        // the strong `Arc` the driver holds below for the shell's lifetime.
        *conn_slot.lock().unwrap() = Arc::downgrade(&conn);

        broker.status(SshPhase::Connected);

        // Open the shell channel on the (possibly shared) connection. This is also
        // the first liveness probe of a *reused* connection: if its transport died
        // silently — a parked forward/loopback accept loop holds an `Arc`, so the
        // dead connection's `Drop` (and `mark_dead`) never ran — the first channel
        // open errors. Self-heal: mark it dead, evict its registry slot, and
        // reconnect fresh once. A fresh connection that fails here is a real error.
        let channel = match conn.open_session_channel().await {
            Ok(channel) => channel,
            Err(e) if reused => {
                log::info!(
                    "reused ssh connection to {}:{} was dead ({e}); reconnecting",
                    spec.host,
                    spec.port
                );
                conn.mark_dead();
                self.evict_connection(conn.key());
                let (fresh, _) = self
                    .open_connection(spec, broker)
                    .await
                    .map_err(|e| format!("{e}"))?;
                conn = fresh;
                *conn_slot.lock().unwrap() = Arc::downgrade(&conn);
                conn.open_session_channel()
                    .await
                    .map_err(|e| format!("open shell channel failed: {e}"))?
            }
            Err(e) => return Err(format!("open shell channel failed: {e}")),
        };

        // Establish the profile's preconfigured forwards (FR-F2) now that the
        // connection is authenticated *and* confirmed live. Failures are non-fatal —
        // each surfaces as a `ForwardStatus::Error` on the forward row, never a
        // killed session.
        for rule in &spec.forwards {
            self.forwards.establish(pane_id, conn.clone(), rule).await;
        }

        let (pw, ph) = (
            u32::from(size.cols).saturating_mul(u32::from(size.cell_w)),
            u32::from(size.rows).saturating_mul(u32::from(size.cell_h)),
        );
        channel
            .request_pty(
                false,
                &spec.term,
                u32::from(size.cols),
                u32::from(size.rows),
                pw,
                ph,
                &sane_terminal_modes(),
            )
            .await
            .map_err(|e| format!("pty-req failed: {e}"))?;

        if spec.agent_forward {
            // Best effort: some servers refuse; a refusal shouldn't abort the shell.
            let _ = channel.agent_forward(false).await;
        }

        channel
            .request_shell(true)
            .await
            .map_err(|e| format!("shell request failed: {e}"))?;

        // Login script: each line verbatim + newline, in order, no expect-logic.
        for line in &spec.login_script {
            let mut bytes = line.clone().into_bytes();
            bytes.push(b'\n');
            let _ = channel.data(&bytes[..]).await;
        }

        // Hand the channel to the pump. `conn` moves in so the shared connection
        // stays alive for this shell's lifetime (and remains reusable meanwhile).
        drive_channel(channel, data_tx, cmd_rx, conn).await;
        Ok(())
    }

    /// Drop a connection key's registry slot so the next `open_connection` for it
    /// establishes a fresh connection instead of upgrading a stale `Weak`. Called
    /// by the self-healing reuse path when a reused connection turns out dead.
    fn evict_connection(&self, key: &ConnectionKey) {
        self.conns.lock().unwrap().remove(key);
    }

    /// Establish (or reuse) the connection for `spec`, recursing through the jump
    /// chain. Boxed because it is `async`-recursive. The returned `bool` is `true`
    /// when an existing connection was reused (no fresh authentication) — the
    /// caller uses it to self-heal: a reused connection whose transport silently
    /// died errors on its first channel open, and only then is it worth evicting
    /// and reconnecting.
    fn open_connection<'a>(
        &'a self,
        spec: &'a NativeSshSpec,
        broker: &'a Arc<PromptBroker>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<(Arc<SshConnection>, bool)>> + Send + 'a>> {
        Box::pin(async move {
            let key = ConnectionKey::from_spec(spec);
            let slot: ConnSlot = {
                let mut map = self.conns.lock().unwrap();
                map.entry(key.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(Weak::new())))
                    .clone()
            };
            let mut guard = slot.lock().await;
            if let Some(conn) = guard.upgrade() {
                if conn.is_alive() {
                    // Reuse: a new channel on the existing authenticated connection.
                    return Ok((conn, true));
                }
            }

            // Establish the jump connection first (recursively) so its
            // `direct-tcpip` channel can be this connection's transport — unless
            // a ProxyCommand is also configured: it outranks the jump in
            // `build_transport`, and establishing (and interactively
            // authenticating) a jump connection that would then be discarded
            // wastes the user's prompts.
            let has_proxy_command =
                matches!(&spec.proxy, crate::daemon::protocol::SshProxy::Command(_));
            let jump = match &spec.jump {
                Some(jump_spec) if !has_proxy_command => {
                    Some(self.open_connection(jump_spec, broker).await?.0)
                }
                _ => None,
            };

            // Transport + SSH handshake under the connect-timeout budget. Auth is
            // deliberately outside it (see `run_session`).
            let budget = spec
                .connect_timeout_s
                .filter(|v| *v > 0)
                .map(|v| Duration::from_secs(u64::from(v)))
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
            // The connection's Remote-forward table, shared with its handler so
            // incoming `forwarded-tcpip` channels resolve to a local target (WS4).
            let remote_forwards = RemoteForwardTable::default();
            let handler = ClientHandler {
                host: spec.host.clone(),
                port: spec.port,
                verify_host_keys: spec.verify_host_keys,
                skip_banner: spec.skip_banner,
                broker: broker.clone(),
                remote_forwards: remote_forwards.clone(),
            };
            let handshake = async {
                let transport = connect::build_transport(spec, jump).await?;
                let config = connect::build_config(spec);
                russh::client::connect_stream(config, transport, handler)
                    .await
                    .map_err(|e| anyhow::anyhow!("ssh handshake failed: {e}"))
            };
            // Watchdog rather than a flat `timeout(budget, ...)`: russh raises the
            // host-key confirmation *inside* connect_stream (via
            // `check_server_key`), and the user reading a fingerprint must not
            // race the network timeout. Ticks are only billed against the budget
            // while no broker prompt is pending; the broker's own per-prompt
            // timeout still bounds an unanswered dialog.
            let mut handshake = std::pin::pin!(handshake);
            let mut remaining = budget;
            const TICK: Duration = Duration::from_millis(200);
            let mut handle = loop {
                match tokio::time::timeout(TICK, handshake.as_mut()).await {
                    Ok(Ok(h)) => break h,
                    Ok(Err(e)) => return Err(e),
                    Err(_) if broker.has_pending() => {}
                    Err(_) => {
                        remaining = remaining.saturating_sub(TICK);
                        if remaining.is_zero() {
                            return Err(anyhow::anyhow!("connection timed out"));
                        }
                    }
                }
            };

            broker.status(SshPhase::Authenticating);
            auth::authenticate(&mut handle, spec, broker)
                .await
                .map_err(anyhow::Error::msg)?;

            let conn = SshConnection::new(handle, key, remote_forwards);
            *guard = Arc::downgrade(&conn);
            Ok((conn, false))
        })
    }
}

/// A conservative set of PTY modes for the shell channel — an interactive TTY
/// with canonical input, echo, and signal handling on, and standard baud codes.
/// The remote line discipline uses these as its starting point.
fn sane_terminal_modes() -> Vec<(Pty, u32)> {
    vec![
        (Pty::ISIG, 1),
        (Pty::ICANON, 1),
        (Pty::ECHO, 1),
        (Pty::ECHOE, 1),
        (Pty::ECHOK, 1),
        (Pty::ICRNL, 1),
        (Pty::OPOST, 1),
        (Pty::ONLCR, 1),
        (Pty::TTY_OP_ISPEED, 38400),
        (Pty::TTY_OP_OSPEED, 38400),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{SshAuthMode, SshProxy};

    fn base_spec() -> NativeSshSpec {
        NativeSshSpec {
            host: "h".into(),
            port: 22,
            user: "u".into(),
            auth_mode: SshAuthMode::Auto,
            identity_files: vec![],
            agent_forward: false,
            password: None,
            key_passphrases: None,
            proxy: SshProxy::None,
            jump: None,
            forwards: vec![],
            keepalive_interval_s: None,
            keepalive_count_max: None,
            connect_timeout_s: None,
            algorithms: Default::default(),
            x11: false,
            term: "xterm-256color".into(),
            verify_host_keys: true,
            skip_banner: false,
            login_script: vec![],
            display_name: None,
            profile_id: None,
        }
    }

    #[test]
    fn connection_key_distinguishes_user_host_port_and_proxy() {
        let a = ConnectionKey::from_spec(&base_spec());
        let mut b = base_spec();
        b.user = "other".into();
        assert_ne!(a, ConnectionKey::from_spec(&b));

        let mut c = base_spec();
        c.proxy = SshProxy::Socks {
            host: "p".into(),
            port: 1080,
        };
        assert_ne!(a, ConnectionKey::from_spec(&c));

        // Identical connection params → identical key (reuse).
        assert_eq!(a, ConnectionKey::from_spec(&base_spec()));
    }

    #[test]
    fn evict_connection_clears_the_registry_slot() {
        // The self-heal path evicts a dead connection's key so the next
        // `open_connection` establishes fresh instead of upgrading a stale `Weak`.
        // Exercise just the registry map — no live server needed.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build test runtime");
        let mgr = SshManager {
            runtime,
            conns: Mutex::new(HashMap::new()),
            forwards: SshForwardRegistry::default(),
        };
        let key = ConnectionKey::from_spec(&base_spec());
        mgr.conns
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::new(tokio::sync::Mutex::new(Weak::new())));
        assert!(mgr.conns.lock().unwrap().contains_key(&key));

        mgr.evict_connection(&key);
        assert!(
            !mgr.conns.lock().unwrap().contains_key(&key),
            "evicted key must be gone so the next open creates a new entry"
        );
    }

    #[test]
    fn connection_key_includes_jump_chain() {
        let mut with_jump = base_spec();
        with_jump.jump = Some(Box::new(base_spec()));
        assert_ne!(
            ConnectionKey::from_spec(&base_spec()),
            ConnectionKey::from_spec(&with_jump)
        );
    }
}
