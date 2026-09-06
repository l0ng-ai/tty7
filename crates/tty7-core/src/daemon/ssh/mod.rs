pub mod broker;
pub mod forward;
pub mod known_hosts;
pub mod session;
pub mod sftp;
pub mod workspace;

mod auth;
mod connect;
mod handler;

#[cfg(test)]
pub(crate) mod test_support;

pub use connect::ProcessStream;

pub use broker::PromptBroker;
pub use forward::SshForwardRegistry;
pub use session::{ChannelCmd, SharedConnection, SshConnection, SshSessionHandle};

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use russh::{ChannelMsg, Pty};

use crate::daemon::protocol::{
    AuthPromptKind, AuthResponse, LoopbackForward, ManagedForward, NativeSshSpec, SshForwardRule,
    SshPhase, SshTestNeed, SshTestReport, WinSize,
};
use crate::daemon::remote_link::{self, RemoteEntry, RemoteLink};
use crate::daemon::router::{RouteChannel, RouteSetup};
use crate::daemon::shell_integration::remote;

use forward::RemoteForwardTable;
use handler::ClientHandler;
use session::drive_channel;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ConnectionKey(String);

impl ConnectionKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }

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

type ConnSlot = Arc<tokio::sync::Mutex<Weak<SshConnection>>>;

pub struct SshManager {
    runtime: tokio::runtime::Runtime,
    conns: Mutex<HashMap<ConnectionKey, ConnSlot>>,
    forwards: SshForwardRegistry,
    probes: Mutex<HashMap<ConnectionKey, Option<(remote::RemoteShell, String)>>>,
}

impl SshManager {
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
                probes: Mutex::new(HashMap::new()),
            }
        })
    }

    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

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

    pub fn remove_forward(&self, pane_id: u64, forward_id: u64) -> Vec<ManagedForward> {
        self.runtime
            .block_on(self.forwards.remove(pane_id, forward_id))
    }

    pub fn list_forwards(&self, pane_id: u64) -> Vec<ManagedForward> {
        self.forwards.list(pane_id)
    }

    pub fn teardown_pane_forwards(&'static self, pane_id: u64) {
        self.runtime.spawn(async move {
            self.forwards.teardown_pane(pane_id).await;
        });
    }

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
                let line = format!("\r\n\x1b[31mtty7: SSH connection failed: {reason}\x1b[0m\r\n");
                let _ = data_tx.send(line.into_bytes()).await;
            }
        });
    }

    /// Open the connection this spec describes, report what happened, and let
    /// it go. The whole path is the real one — proxy, jump host, host-key
    /// check, authentication — so a pass means the next Connect will work and a
    /// failure carries the same message the pane would have printed.
    ///
    /// Anything the handshake would have *asked* a person is refused on the
    /// spot and reported as what it asked for: a form is nowhere to answer a
    /// password prompt, and hanging on one for two minutes would be a worse
    /// answer than "it got that far and wants your password".
    pub fn test_connection(&'static self, spec: &NativeSshSpec) -> SshTestReport {
        let budget = spec
            .connect_timeout_s
            .filter(|v| *v > 0)
            .map(|v| Duration::from_secs(u64::from(v)))
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        let asked: Arc<Mutex<Option<SshTestNeed>>> = Arc::new(Mutex::new(None));
        let broker = declining_broker(Arc::clone(&asked));
        let started = std::time::Instant::now();

        let outcome = self.runtime.block_on(async {
            let dial = self.open_connection_reusing(spec, &broker, false);
            match tokio::time::timeout(budget, dial).await {
                Ok(result) => result.map_err(|e| format!("{e}")),
                Err(_) => Err("connection timed out".to_string()),
            }
        });
        let elapsed_ms = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;

        // A test holds nothing open and leaves nothing behind: it never entered
        // the connection cache, so dropping the only `Arc` closes it.
        match outcome {
            Ok((conn, _reused)) => {
                drop(conn);
                SshTestReport::Authenticated { elapsed_ms }
            }
            Err(reason) => match asked.lock().ok().and_then(|a| *a) {
                Some(need) => SshTestReport::NeedsInput { need, elapsed_ms },
                None => SshTestReport::Failed { reason },
            },
        }
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

        let (mut conn, reused) = self
            .open_connection(spec, broker)
            .await
            .map_err(|e| format!("{e}"))?;

        *conn_slot.lock().unwrap() = Arc::downgrade(&conn);

        broker.status(SshPhase::Connected);

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
            let _ = channel.agent_forward(false).await;
        }

        let bootstrap = match spec.shell_integration {
            true => self.remote_bootstrap(&conn).await,
            false => None,
        };
        match bootstrap {
            Some(script) => channel
                .exec(true, script)
                .await
                .map_err(|e| format!("shell request failed: {e}"))?,
            None => channel
                .request_shell(true)
                .await
                .map_err(|e| format!("shell request failed: {e}"))?,
        }

        for line in &spec.login_script {
            let mut bytes = line.clone().into_bytes();
            bytes.push(b'\n');
            let _ = channel.data(&bytes[..]).await;
        }

        drive_channel(channel, data_tx, cmd_rx, conn).await;
        Ok(())
    }

    pub async fn open_remote_link(
        &self,
        spec: &NativeSshSpec,
        setup: &RouteSetup,
        server_command: Option<&str>,
    ) -> anyhow::Result<(RemoteLink, Arc<SshConnection>)> {
        let (conn, _reused) = self.open_connection(spec, &setup.broker).await?;

        // An explicit endpoint must not prepare the account's default daemon
        // or reuse its cached streamlocal socket. Otherwise two routes to
        // different configurations on one SSH host silently land on one server.
        if let Some(explicit) = server_command {
            let command = setup.channel.bridge_command(explicit);
            let link = exec_remote_link(&conn, &command).await?;
            return Ok((link, conn));
        }

        let installed = {
            let install_conn = conn.clone();
            setup
                .blocking(move || crate::daemon::install::ensure_remote_server(&install_conn))
                .await??
        };

        let base = format!(
            "{} --stdio",
            crate::daemon::install::shell_quote(&installed)
        );
        let command = setup.channel.bridge_command(&base);

        let entry = match setup.channel {
            RouteChannel::Pane => RemoteEntry::SessionExec {
                command: command.clone(),
            },
            RouteChannel::Control => {
                conn.remote_entry_or_init(|| async {
                    let env = probe_remote_env(&conn).await;
                    let socket = env.as_ref().and_then(remote_link::remote_control_socket);
                    remote_link::choose_entry(socket.as_deref(), true, &command)
                })
                .await
            }
        };

        if let RemoteEntry::StreamLocal { socket } = &entry {
            match conn.open_direct_streamlocal(socket).await {
                Ok(channel) => return Ok((RemoteLink::stream_local(channel), conn)),
                Err(e) => {
                    log::info!(
                        "ssh {:?}: direct-streamlocal to {socket} refused ({e}); \
                         falling back to `{command}`",
                        conn.key()
                    );
                    conn.set_remote_entry(remote_link::choose_entry(Some(socket), false, &command))
                        .await;
                }
            }
        }

        let link = exec_remote_link(&conn, &command).await?;
        Ok((link, conn))
    }

    pub async fn restart_remote_server(
        &self,
        spec: &NativeSshSpec,
        setup: &RouteSetup,
        legacy_stop: bool,
    ) -> anyhow::Result<()> {
        let (conn, _reused) = self.open_connection(spec, &setup.broker).await?;
        setup
            .blocking(move || {
                crate::daemon::install::restart_remote_daemon_consenting(&conn, legacy_stop)
            })
            .await??;
        Ok(())
    }

    pub async fn replace_remote_server(
        &self,
        spec: &NativeSshSpec,
        setup: &RouteSetup,
        legacy_stop: bool,
    ) -> anyhow::Result<()> {
        let (conn, _reused) = self.open_connection(spec, &setup.broker).await?;
        setup
            .blocking(move || {
                crate::daemon::install::replace_remote_server_consenting(&conn, legacy_stop)
            })
            .await??;
        Ok(())
    }

    pub fn open_remote_link_blocking(
        &self,
        spec: &NativeSshSpec,
        setup: &RouteSetup,
        server_command: Option<&str>,
    ) -> anyhow::Result<(RemoteLink, Arc<SshConnection>)> {
        self.runtime
            .block_on(self.open_remote_link(spec, setup, server_command))
    }

    fn evict_connection(&self, key: &ConnectionKey) {
        self.conns.lock().unwrap().remove(key);
    }

    pub fn routes(&self) -> Vec<crate::daemon::control::RouteInfo> {
        let conns = self.conns.lock().unwrap();
        let mut routes: Vec<_> = conns
            .iter()
            .map(|(key, slot)| {
                let connected = match slot.try_lock() {
                    Ok(weak) => weak.upgrade().is_some_and(|conn| conn.is_alive()),
                    // The slot is held by whoever is opening or using this link
                    // right now. Busy is not down — and callers act on this:
                    // the CLI's `-m <machine>` refuses to route over a link it
                    // is told is down, so guessing false here fails a perfectly
                    // live connection the moment it gets used. SshConnection::
                    // is_alive resolves its own lock contention the same way.
                    Err(_) => true,
                };
                crate::daemon::control::RouteInfo {
                    key: key.as_str().to_string(),
                    kind: "ssh".to_string(),
                    connected,
                }
            })
            .collect();
        routes.sort_by(|a, b| a.key.cmp(&b.key));
        routes
    }

    async fn remote_bootstrap(&self, conn: &Arc<SshConnection>) -> Option<String> {
        let key = conn.key().clone();
        let cached = { self.probes.lock().unwrap().get(&key).cloned() };
        let probed = match cached {
            Some(hit) => hit,
            None => {
                let probed = probe_remote_shell(conn).await;
                match &probed {
                    Some((shell, path)) => {
                        log::debug!("ssh {key:?}: remote shell {shell:?} at {path}")
                    }
                    None => log::debug!("ssh {key:?}: no remote shell integration"),
                }
                // A probe the link would not carry says nothing about the
                // shell. Remembered, its nothing would keep integration off
                // this host for the rest of the run, fresh links included.
                if probed.is_some() || conn.is_alive() {
                    self.probes.lock().unwrap().insert(key, probed.clone());
                }
                probed
            }
        };
        probed.map(|(shell, path)| remote::bootstrap_command(shell, &path))
    }

    fn open_connection<'a>(
        &'a self,
        spec: &'a NativeSshSpec,
        broker: &'a Arc<PromptBroker>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<(Arc<SshConnection>, bool)>> + Send + 'a>> {
        self.open_connection_reusing(spec, broker, true)
    }

    /// `reuse` is what separates opening a session from testing one. A session
    /// is glad to ride an existing connection; a test that did would report on
    /// the credentials that connection was made with, not the ones in the form
    /// — a password typed wrong would come back green. So a test dials its own
    /// and leaves the cache to the sessions.
    ///
    /// It leaves the cache's *lock* alone too. The slot is held for the whole
    /// handshake, so a test that took it would stall every Connect to the same
    /// host behind a connection it is not going to leave them — and, waiting
    /// its turn behind a session already dialling, would spend its own budget
    /// on the queue and come back "connection timed out" about a host that
    /// answers fine.
    fn open_connection_reusing<'a>(
        &'a self,
        spec: &'a NativeSshSpec,
        broker: &'a Arc<PromptBroker>,
        reuse: bool,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<(Arc<SshConnection>, bool)>> + Send + 'a>> {
        Box::pin(async move {
            let key = ConnectionKey::from_spec(spec);
            let mut guard = match reuse {
                true => {
                    let slot: ConnSlot = {
                        let mut map = self.conns.lock().unwrap();
                        map.entry(key.clone())
                            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(Weak::new())))
                            .clone()
                    };
                    let guard = slot.lock_owned().await;
                    if let Some(conn) = guard.upgrade()
                        && conn.is_alive()
                    {
                        return Ok((conn, true));
                    }
                    Some(guard)
                }
                false => None,
            };

            let has_proxy_command =
                matches!(&spec.proxy, crate::daemon::protocol::SshProxy::Command(_));
            let jump = match &spec.jump {
                Some(jump_spec) if !has_proxy_command => {
                    Some(self.open_connection(jump_spec, broker).await?.0)
                }
                _ => None,
            };

            let budget = spec
                .connect_timeout_s
                .filter(|v| *v > 0)
                .map(|v| Duration::from_secs(u64::from(v)))
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
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
            if let Some(guard) = guard.as_mut() {
                **guard = Arc::downgrade(&conn);
            }
            Ok((conn, false))
        })
    }
}

async fn exec_remote_link(conn: &SshConnection, command: &str) -> anyhow::Result<RemoteLink> {
    let channel = conn
        .open_session_channel()
        .await
        .map_err(|e| anyhow::anyhow!("open remote workspace channel failed: {e}"))?;
    channel
        .exec(false, command.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("exec `{command}` on the remote failed: {e}"))?;
    Ok(RemoteLink::session_exec(channel))
}

/// A broker that answers every prompt with "cancelled" the moment it is asked,
/// and remembers what the first ask was for.
///
/// It answers from inside its own emit closure, which works because
/// [`PromptBroker::prompt`] files the waiting sender before it emits — so the
/// reply lands on a channel that is already there, and nothing waits out the
/// two-minute prompt timeout or the fifteen-second delivery window.
fn declining_broker(asked: Arc<Mutex<Option<SshTestNeed>>>) -> Arc<PromptBroker> {
    let back: Arc<OnceLock<Weak<PromptBroker>>> = Arc::new(OnceLock::new());
    let emit_back = Arc::clone(&back);
    let broker = PromptBroker::new(Box::new(move |msg| {
        let crate::daemon::protocol::DaemonMsg::AuthPrompt { request_id, prompt } = msg else {
            // Status and banner frames are not questions; drop them.
            return true;
        };
        let need = match prompt {
            AuthPromptKind::Password { .. } => SshTestNeed::Password,
            AuthPromptKind::KeyPassphrase { .. } => SshTestNeed::KeyPassphrase,
            AuthPromptKind::KeyboardInteractive { .. } => SshTestNeed::KeyboardInteractive,
            AuthPromptKind::HostKeyUnknown { .. } => SshTestNeed::HostKeyDecision,
            AuthPromptKind::HostKeyChanged { .. } => SshTestNeed::HostKeyChanged,
            // Delivered with request_id 0 and never waited on.
            AuthPromptKind::Banner { .. } => return true,
        };
        if let Ok(mut slot) = asked.lock() {
            slot.get_or_insert(need);
        }
        if let Some(broker) = emit_back.get().and_then(Weak::upgrade) {
            broker.deliver(request_id, AuthResponse::Cancelled);
        }
        true
    }));
    let _ = back.set(Arc::downgrade(&broker));
    broker
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

const PROBE_OUTPUT_LIMIT: usize = 8 * 1024;

async fn probe_remote_shell(conn: &SshConnection) -> Option<(remote::RemoteShell, String)> {
    let mut channel = conn.open_command_channel().await.ok()?;
    channel.exec(true, remote::PROBE_COMMAND).await.ok()?;

    let mut out: Vec<u8> = Vec::new();
    let collect = async {
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                    out.extend_from_slice(&data);
                    if out.len() >= PROBE_OUTPUT_LIMIT {
                        break;
                    }
                }
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
    };
    let _ = tokio::time::timeout(PROBE_TIMEOUT, collect).await;

    remote::parse_probe(&String::from_utf8_lossy(&out))
}

async fn probe_remote_env(conn: &SshConnection) -> Option<remote_link::RemoteEnv> {
    let mut channel = conn.open_command_channel().await.ok()?;
    channel
        .exec(true, remote_link::REMOTE_ENV_PROBE)
        .await
        .ok()?;

    let mut out: Vec<u8> = Vec::new();
    let collect = async {
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                    out.extend_from_slice(&data);
                    if out.len() >= PROBE_OUTPUT_LIMIT {
                        break;
                    }
                }
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
    };
    let _ = tokio::time::timeout(PROBE_TIMEOUT, collect).await;

    let env = remote_link::RemoteEnv::parse_probe(&String::from_utf8_lossy(&out));
    (env != remote_link::RemoteEnv::default()).then_some(env)
}

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
    use super::test_support::{Exec, FakeSshd, base_spec};
    use super::*;
    use crate::daemon::protocol::{SshAuthMode, SshProxy};

    /// A manager of its own, so nothing here touches the global cache. Its
    /// runtime hosts the fake server and the connection under test.
    fn manager() -> SshManager {
        SshManager {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test runtime"),
            conns: Mutex::new(HashMap::new()),
            forwards: SshForwardRegistry::default(),
            probes: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn a_probe_the_link_would_not_carry_is_not_remembered() {
        let mgr = manager();
        mgr.runtime.block_on(async {
            let sshd = FakeSshd::connect(Exec::Hangs, Some(0)).await;
            assert!(mgr.remote_bootstrap(&sshd.conn).await.is_none());
            assert!(!sshd.conn.is_alive(), "a refused session retires the link");
            assert!(
                !mgr.probes.lock().unwrap().contains_key(sshd.conn.key()),
                "the next session, on a fresh link, must ask again"
            );
        });
    }

    #[test]
    fn explicit_routes_never_install_or_reuse_another_daemons_socket() {
        use tokio::io::AsyncReadExt as _;
        let mgr = manager();
        mgr.runtime.block_on(async {
            let sshd = FakeSshd::connect(Exec::Exits, None).await;
            let mut spec = base_spec();
            spec.host = "127.0.0.1".into();
            spec.port = sshd
                .conn
                .key()
                .as_str()
                .rsplit(':')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            mgr.conns.lock().unwrap().insert(
                ConnectionKey::from_spec(&spec),
                Arc::new(tokio::sync::Mutex::new(Arc::downgrade(&sshd.conn))),
            );
            sshd.conn
                .set_remote_entry(RemoteEntry::StreamLocal {
                    socket: "/default-do-not-touch.sock".into(),
                })
                .await;
            for channel in [RouteChannel::Control, RouteChannel::Pane] {
                let setup = RouteSetup::unattended(channel);
                let (mut link, conn) = mgr
                    .open_remote_link(
                        &spec,
                        &setup,
                        Some("test-candidate --config-dir /private --stdio"),
                    )
                    .await
                    .unwrap();
                assert!(Arc::ptr_eq(&conn, &sshd.conn));
                let mut bytes = Vec::new();
                link.read_to_end(&mut bytes).await.unwrap();
                assert_eq!(bytes, b"ok\n");
            }
            assert_eq!(
                sshd.commands(),
                vec![
                    "test-candidate --config-dir /private --stdio",
                    "test-candidate --config-dir /private --stdio --pane",
                ]
            );
            assert_eq!(
                sshd.opened(),
                2,
                "no install, home probe or default endpoint fallback"
            );
        });
    }

    #[test]
    fn a_probe_answered_with_no_integration_is_remembered() {
        let mgr = manager();
        mgr.runtime.block_on(async {
            let sshd = FakeSshd::connect(Exec::Exits, None).await;
            assert!(mgr.remote_bootstrap(&sshd.conn).await.is_none());
            assert!(sshd.conn.is_alive());
            assert!(mgr.probes.lock().unwrap().contains_key(sshd.conn.key()));
            sshd.wait_for_closed(1).await;
            assert_eq!(sshd.opened(), 1);
        });
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

        assert_eq!(a, ConnectionKey::from_spec(&base_spec()));
    }

    #[test]
    fn the_key_string_names_the_whole_chain() {
        assert_eq!(ConnectionKey::from_spec(&base_spec()).as_str(), "u@h:22");

        let mut jumped = base_spec();
        let mut bastion = base_spec();
        bastion.host = "bastion".into();
        jumped.jump = Some(Box::new(bastion));
        assert_eq!(
            ConnectionKey::from_spec(&jumped).as_str(),
            "u@h:22|jump:u@bastion:22"
        );
    }

    #[test]
    fn evict_connection_clears_the_registry_slot() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build test runtime");
        let mgr = SshManager {
            runtime,
            conns: Mutex::new(HashMap::new()),
            forwards: SshForwardRegistry::default(),
            probes: Mutex::new(HashMap::new()),
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
    fn routes_names_each_held_connection_with_its_liveness() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build test runtime");
        let mgr = SshManager {
            runtime,
            conns: Mutex::new(HashMap::new()),
            forwards: SshForwardRegistry::default(),
            probes: Mutex::new(HashMap::new()),
        };
        assert!(mgr.routes().is_empty());

        let mut other = base_spec();
        other.host = "build-box".into();
        for spec in [&base_spec(), &other] {
            mgr.conns.lock().unwrap().insert(
                ConnectionKey::from_spec(spec),
                Arc::new(tokio::sync::Mutex::new(Weak::new())),
            );
        }

        let routes = mgr.routes();
        let keys: Vec<&str> = routes.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["u@build-box:22", "u@h:22"],
            "every held connection is listed, in a stable order"
        );
        for route in &routes {
            assert_eq!(route.kind, "ssh");
            assert!(
                !route.connected,
                "a dropped connection must read as disconnected, not vanish"
            );
        }
    }

    #[test]
    fn a_busy_link_reads_as_connected_rather_than_down() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build test runtime");
        let mgr = SshManager {
            runtime,
            conns: Mutex::new(HashMap::new()),
            forwards: SshForwardRegistry::default(),
            probes: Mutex::new(HashMap::new()),
        };
        let slot: ConnSlot = Arc::new(tokio::sync::Mutex::new(Weak::new()));
        mgr.conns
            .lock()
            .unwrap()
            .insert(ConnectionKey::from_spec(&base_spec()), slot.clone());

        // Someone is mid-operation on this link: opening a channel, running the
        // remote bootstrap probe, anything that holds the slot for a moment.
        let _busy = slot.try_lock().expect("nobody else holds it in this test");

        let routes = mgr.routes();
        assert_eq!(routes.len(), 1, "a busy link must still be listed");
        assert!(
            routes[0].connected,
            "a link whose slot is momentarily held is busy, not down — calling it \
             down makes `tty7 -m <machine>` refuse to route over a live connection"
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

    #[test]
    #[ignore = "requires a live SSH server and local GSSAPI credentials"]
    fn live_gssapi_connects_and_opens_a_channel() {
        let host = std::env::var("TTY7_LIVE_SSH_HOST").expect("TTY7_LIVE_SSH_HOST");
        let user = std::env::var("TTY7_LIVE_SSH_USER").expect("TTY7_LIVE_SSH_USER");
        let port = std::env::var("TTY7_LIVE_SSH_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(22);

        let mut spec = base_spec();
        spec.host = host;
        spec.user = user;
        spec.port = port;
        spec.auth_mode = SshAuthMode::Gssapi;
        spec.connect_timeout_s = Some(10);
        spec.verify_host_keys = false;

        let manager = SshManager::global();
        let broker = PromptBroker::new(Box::new(|_| true));
        manager.runtime.block_on(async {
            let (conn, reused) = manager
                .open_connection(&spec, &broker)
                .await
                .expect("native GSSAPI connection");
            assert!(!reused);
            conn.open_session_channel()
                .await
                .expect("open session channel");
            conn.mark_dead();
            manager.evict_connection(conn.key());
        });
    }

    /// The broker a connection test hands the handshake. If it ever waited for
    /// a real answer, a test against a password host would sit there for two
    /// minutes with a spinner on it; it has to come back at once, and it has to
    /// say which question it turned down.
    #[test]
    fn a_test_broker_declines_every_prompt_at_once_and_remembers_what_was_asked() {
        let asked = Arc::new(Mutex::new(None));
        let broker = declining_broker(Arc::clone(&asked));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");

        let answer = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(1),
                broker.prompt(AuthPromptKind::Password {
                    user: "u".into(),
                    host: "h".into(),
                }),
            )
            .await
        });
        assert_eq!(
            answer,
            Ok(AuthResponse::Cancelled),
            "a prompt nobody can answer is declined, not waited on"
        );
        assert_eq!(*asked.lock().unwrap(), Some(SshTestNeed::Password));

        // The first question is the one worth reporting: a host key that has to
        // be reviewed is why the password was never reached.
        let answer = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(1),
                broker.prompt(AuthPromptKind::KeyboardInteractive {
                    name: "2FA".into(),
                    instructions: String::new(),
                    prompts: vec![],
                    stored_rejected: false,
                }),
            )
            .await
        });
        assert_eq!(answer, Ok(AuthResponse::Cancelled));
        assert_eq!(*asked.lock().unwrap(), Some(SshTestNeed::Password));

        // A banner is not a question; it must not be mistaken for one.
        let fresh = Arc::new(Mutex::new(None));
        let quiet = declining_broker(Arc::clone(&fresh));
        quiet.banner("welcome".into());
        assert_eq!(*fresh.lock().unwrap(), None);
    }
}
