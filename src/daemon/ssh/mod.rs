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
pub mod known_hosts;
pub mod session;

mod auth;
mod connect;
mod handler;

pub use broker::PromptBroker;
pub use session::{ChannelCmd, SharedConnection, SshConnection, SshSessionHandle};

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use russh::Pty;

use crate::daemon::protocol::{NativeSshSpec, SshPhase, WinSize};

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
            }
        })
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
        spec: Box<NativeSshSpec>,
        size: WinSize,
        broker: Arc<PromptBroker>,
        data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
        cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ChannelCmd>,
        conn_slot: SharedConnection,
    ) {
        self.runtime.spawn(async move {
            if let Err(reason) = self
                .run_session(&spec, size, &broker, data_tx.clone(), cmd_rx, &conn_slot)
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
        let conn = self
            .open_connection(spec, broker)
            .await
            .map_err(|e| format!("{e}"))?;

        // Publish the connection so the pane (and WS4/WS5) can open further
        // channels on it. A `Weak`, so this never keeps the connection alive past
        // the strong `Arc` the driver holds below for the shell's lifetime.
        *conn_slot.lock().unwrap() = Arc::downgrade(&conn);

        broker.status(SshPhase::Connected);

        // Open the shell channel on the (possibly shared) connection.
        let channel = conn
            .open_session_channel()
            .await
            .map_err(|e| format!("open shell channel failed: {e}"))?;

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

    /// Establish (or reuse) the connection for `spec`, recursing through the jump
    /// chain. Boxed because it is `async`-recursive.
    fn open_connection<'a>(
        &'a self,
        spec: &'a NativeSshSpec,
        broker: &'a Arc<PromptBroker>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Arc<SshConnection>>> + Send + 'a>> {
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
                    return Ok(conn);
                }
            }

            // Establish the jump connection first (recursively) so its
            // `direct-tcpip` channel can be this connection's transport.
            let jump = match &spec.jump {
                Some(jump_spec) => Some(self.open_connection(jump_spec, broker).await?),
                None => None,
            };

            // Transport + SSH handshake under the connect-timeout budget. Auth is
            // deliberately outside it (see `run_session`).
            let budget = spec
                .connect_timeout_s
                .filter(|v| *v > 0)
                .map(|v| Duration::from_secs(u64::from(v)))
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
            let handler = ClientHandler {
                host: spec.host.clone(),
                port: spec.port,
                verify_host_keys: spec.verify_host_keys,
                skip_banner: spec.skip_banner,
                broker: broker.clone(),
            };
            let handshake = async {
                let transport = connect::build_transport(spec, jump).await?;
                let config = connect::build_config(spec);
                russh::client::connect_stream(config, transport, handler)
                    .await
                    .map_err(|e| anyhow::anyhow!("ssh handshake failed: {e}"))
            };
            let mut handle = match tokio::time::timeout(budget, handshake).await {
                Ok(Ok(h)) => h,
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(anyhow::anyhow!("connection timed out")),
            };

            broker.status(SshPhase::Authenticating);
            auth::authenticate(&mut handle, spec, broker)
                .await
                .map_err(anyhow::Error::msg)?;

            let conn = SshConnection::new(handle, key);
            *guard = Arc::downgrade(&conn);
            Ok(conn)
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
    fn connection_key_includes_jump_chain() {
        let mut with_jump = base_spec();
        with_jump.jump = Some(Box::new(base_spec()));
        assert_ne!(
            ConnectionKey::from_spec(&base_spec()),
            ConnectionKey::from_spec(&with_jump)
        );
    }
}
