pub mod broker;
pub mod forward;
pub mod known_hosts;
pub mod session;
pub mod sftp;
pub mod workspace;

mod auth;
mod connect;
mod handler;

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

pub(crate) struct SshManager {
    runtime: tokio::runtime::Runtime,
    conns: Mutex<HashMap<ConnectionKey, ConnSlot>>,
    forwards: SshForwardRegistry,
    /// Memoized remote shell-integration probes, keyed like connections. A
    /// present `None` means "probed, nothing to inject" — cached just as firmly
    /// as a hit so an unintegrable host isn't re-probed on every new tab. See
    /// [`SshManager::remote_bootstrap`].
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

    pub(crate) fn teardown_pane_forwards(&'static self, pane_id: u64) {
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
    pub(crate) fn test_connection(&'static self, spec: &NativeSshSpec) -> SshTestReport {
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

        let installed = {
            let install_conn = conn.clone();
            setup
                .blocking(move || crate::daemon::install::ensure_remote_server(&install_conn))
                .await??
        };

        let base = match server_command {
            Some(explicit) => explicit.to_string(),
            None => format!(
                "{} --stdio",
                crate::daemon::install::shell_quote(&installed)
            ),
        };
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

        let channel = conn
            .open_session_channel()
            .await
            .map_err(|e| anyhow::anyhow!("open remote workspace channel failed: {e}"))?;
        channel
            .exec(false, command.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("exec `{command}` on the remote failed: {e}"))?;
        Ok((RemoteLink::session_exec(channel), conn))
    }

    pub async fn restart_remote_server(
        &self,
        spec: &NativeSshSpec,
        setup: &RouteSetup,
    ) -> anyhow::Result<()> {
        let (conn, _reused) = self.open_connection(spec, &setup.broker).await?;
        setup
            .blocking(move || crate::daemon::install::restart_remote_daemon(&conn))
            .await??;
        Ok(())
    }

    pub async fn replace_remote_server(
        &self,
        spec: &NativeSshSpec,
        setup: &RouteSetup,
    ) -> anyhow::Result<()> {
        let (conn, _reused) = self.open_connection(spec, &setup.broker).await?;
        setup
            .blocking(move || crate::daemon::install::replace_remote_server(&conn))
            .await??;
        Ok(())
    }

    /// Unused: every caller opens the link through the async path.
    #[allow(dead_code)]
    pub(crate) fn open_remote_link_blocking(
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
            // Only an answer is remembered. This map is on the process-wide
            // `SshManager` and nothing ever evicts from it, so caching a probe
            // that failed would spend the rest of the daemon's life claiming a
            // host has no shell integration because one channel open, one
            // exec, or one five-second read went badly. Reconnecting would not
            // clear it either.
            None => match probe_remote_shell(conn).await {
                Some(answer) => {
                    match &answer {
                        Some((shell, path)) => {
                            log::debug!("ssh {key:?}: remote shell {shell:?} at {path}")
                        }
                        None => log::debug!("ssh {key:?}: no remote shell integration"),
                    }
                    self.probes.lock().unwrap().insert(key, answer.clone());
                    answer
                }
                None => {
                    log::debug!("ssh {key:?}: shell probe did not answer; will ask again");
                    None
                }
            },
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

/// `None` when the remote never answered, so the caller knows not to remember
/// it. See [`remote::probe_answer`].
async fn probe_remote_shell(conn: &SshConnection) -> Option<Option<(remote::RemoteShell, String)>> {
    let mut channel = conn.open_session_channel().await.ok()?;
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

    remote::probe_answer(&String::from_utf8_lossy(&out))
}

async fn probe_remote_env(conn: &SshConnection) -> Option<remote_link::RemoteEnv> {
    let mut channel = conn.open_session_channel().await.ok()?;
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
            shell_integration: true,
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

    /// The live tests share `SshManager::global()`, and sharing is the point of
    /// it: one asserts that a second `open_connection` is *reused*, and any
    /// other test evicting the same key underneath it makes that false. Run
    /// under `--ignored` they are the only tests running, and they still
    /// overlap with each other, so they take turns.
    static LIVE_SSH: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn live_ssh_turn() -> std::sync::MutexGuard<'static, ()> {
        LIVE_SSH.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Where the live tests point, and with what. Defaults chosen so that a
    /// machine with Remote Login on needs no environment at all.
    fn live_key_spec() -> NativeSshSpec {
        let mut spec = base_spec();
        spec.host = std::env::var("TTY7_LIVE_SSH_HOST").unwrap_or_else(|_| "localhost".into());
        spec.user = std::env::var("TTY7_LIVE_SSH_USER")
            .or_else(|_| std::env::var("USER"))
            .expect("a user to log in as");
        spec.port = std::env::var("TTY7_LIVE_SSH_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(22);
        spec.identity_files = vec![std::env::var("TTY7_LIVE_SSH_KEY").unwrap_or_else(|_| {
            format!(
                "{}/.ssh/id_ed25519",
                std::env::var("HOME").unwrap_or_default()
            )
        })];
        spec.auth_mode = SshAuthMode::PublicKey;
        spec.connect_timeout_s = Some(10);
        spec.verify_host_keys = false;
        spec
    }

    /// The common case, end to end: key auth against a real sshd.
    ///
    /// tty7 speaks SSH itself rather than shelling out, and everything above
    /// this — remote workspaces, the routed pane socket, SFTP — stands on the
    /// connection it makes. The only live test beside it needs Kerberos, which
    /// is the case almost nobody runs, so the case almost everybody runs had no
    /// end-to-end check at all.
    ///
    /// Ignored because it needs a server, like its neighbour. It needs much
    /// less of one: Remote Login on the machine running the test and a key it
    /// accepts, which is a `ssh localhost true` away. Defaults are chosen so
    /// that on such a machine the whole thing is
    /// `cargo test -p tty7-core --lib live_key_auth -- --ignored`.
    ///
    /// `verify_host_keys` is off for the same reason it is off next door: the
    /// point is the transport, and a first connection to a host nobody has
    /// approved would otherwise sit on a prompt no test can answer.
    #[test]
    #[ignore = "requires a live SSH server that accepts a key of yours"]
    fn live_key_auth_connects_and_opens_a_channel() {
        let _turn = live_ssh_turn();
        let spec = live_key_spec();
        let (host, user, key) = (
            spec.host.clone(),
            spec.user.clone(),
            spec.identity_files[0].clone(),
        );
        let port = spec.port;

        let manager = SshManager::global();
        let broker = PromptBroker::new(Box::new(|_| true));
        manager.runtime.block_on(async {
            let (conn, reused) = manager
                .open_connection(&spec, &broker)
                .await
                .unwrap_or_else(|e| panic!("connecting to {user}@{host}:{port} with {key}: {e}"));
            assert!(!reused, "the first connection of a run is not a reuse");
            // Not just that a channel opens: run something and read what comes
            // back. Opening proves the handshake; this proves the transport,
            // which is what every pane on a remote machine is.
            let mut channel = conn
                .open_session_channel()
                .await
                .expect("a channel on a connection that authenticated");
            channel
                .exec(true, "echo tty7-live-check")
                .await
                .expect("exec on the channel");
            let mut said = String::new();
            let mut code = None;
            while let Some(msg) = channel.wait().await {
                match msg {
                    russh::ChannelMsg::Data { ref data } => {
                        said.push_str(&String::from_utf8_lossy(data));
                    }
                    russh::ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    // Deliberately no `break` on `Eof`: openssh sends the exit
                    // status *after* it, so stopping there reads the output and
                    // misses how it ended. `wait` returns `None` when the
                    // channel is really finished, which is the only end worth
                    // waiting for.
                    _ => {}
                }
            }
            assert_eq!(said.trim(), "tty7-live-check", "the far side ran it and answered");
            assert_eq!(code, Some(0), "and said how it went");

            // The second ask has to come back on the same connection: sharing
            // one transport across panes is the reason `SshManager` exists.
            let (again, reused) = manager
                .open_connection(&spec, &broker)
                .await
                .expect("a second connection to the same host");
            assert!(reused, "the second ask should have been given the first one");
            assert_eq!(again.key(), conn.key());

            conn.mark_dead();
            manager.evict_connection(conn.key());
        });
    }

    /// A tunnel through a real server carries bytes both ways.
    ///
    /// `open_direct_tcpip` is what every local forward and the SOCKS proxy are
    /// built on, and what a remote workspace's control link rides. Its own
    /// tests are about the *bookkeeping* — which rule is registered, what the
    /// teardown reports — and none of them puts a byte through a socket. The
    /// far end being this same machine costs nothing here: the channel is a
    /// real one and the server on the other side of it is a real sshd, which
    /// is the part that was never exercised.
    ///
    /// A listener of our own rather than a well-known port, so the test needs
    /// nothing to be running and cannot be fooled by something that is.
    #[test]
    #[ignore = "requires a live SSH server that accepts a key of yours"]
    fn live_direct_tcpip_carries_bytes_both_ways() {
        let _turn = live_ssh_turn();
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let spec = live_key_spec();
        let manager = SshManager::global();
        let broker = PromptBroker::new(Box::new(|_| true));
        manager.runtime.block_on(async {
            // Something for the tunnel to reach: echo one line back, uppercased,
            // so a reply cannot be an accident of buffering.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a loopback listener");
            let port = listener.local_addr().expect("its address").port();
            let served = tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.expect("the tunnel connects");
                let mut buf = [0u8; 64];
                let n = sock.read(&mut buf).await.expect("read what came through");
                let said = String::from_utf8_lossy(&buf[..n]).to_uppercase();
                sock.write_all(said.as_bytes()).await.expect("answer it");
                sock.flush().await.ok();
                said
            });

            let (conn, _) = manager
                .open_connection(&spec, &broker)
                .await
                .expect("a connection to tunnel through");
            let mut channel = conn
                .open_direct_tcpip("127.0.0.1", port)
                .await
                .expect("a direct-tcpip channel to our own listener");
            channel
                .data(&b"tty7-tunnel\n"[..])
                .await
                .expect("write into the tunnel");

            let mut back = String::new();
            while let Some(msg) = channel.wait().await {
                if let russh::ChannelMsg::Data { ref data } = msg {
                    back.push_str(&String::from_utf8_lossy(data));
                    if back.contains('\n') {
                        break;
                    }
                }
            }
            assert_eq!(
                back.trim(),
                "TTY7-TUNNEL",
                "the far side answered through the tunnel"
            );
            assert_eq!(
                served.await.expect("the listener finished").trim(),
                "TTY7-TUNNEL",
                "and had received what was sent"
            );

            conn.mark_dead();
            manager.evict_connection(conn.key());
        });
    }

    /// The SFTP subsystem against a real server: create, list, stat, rename,
    /// remove.
    ///
    /// `SftpManager` had no live coverage of any kind. Its own tests cover the
    /// path arithmetic — `remote_join`, `remote_parent`, `safe_local_name` —
    /// and the transport-failure classifier, none of which needs a server, and
    /// all of which is downstream of a subsystem that was never opened in a
    /// test. The panel is built entirely on these two calls.
    ///
    /// Everything happens under a scratch directory named for this process, on
    /// the far side, and is removed at the end. The far side being this same
    /// machine is what makes that safe to assert about.
    #[test]
    #[ignore = "requires a live SSH server that accepts a key of yours"]
    fn live_sftp_lists_and_edits_a_real_directory() {
        let _turn = live_ssh_turn();
        let spec = live_key_spec();
        let manager = SshManager::global();
        let broker = PromptBroker::new(Box::new(|_| true));
        let conn = manager
            .runtime
            .block_on(async { manager.open_connection(&spec, &broker).await })
            .expect("a connection to run sftp over")
            .0;

        let sftp = crate::daemon::ssh::sftp::SftpManager::global();
        let root = format!(
            "{}/tty7-sftp-live-{}",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
            std::process::id()
        );
        let file = format!("{root}/hello.txt");
        let moved = format!("{root}/moved.txt");

        use crate::daemon::protocol::{SftpEntryKind, SftpOp, SftpOpResult};
        let ok = |op: SftpOp| match sftp.op(&conn, &op) {
            SftpOpResult::Done => {}
            other => panic!("{op:?} answered {other:?}"),
        };

        ok(SftpOp::Mkdir { path: root.clone() });
        ok(SftpOp::CreateFile { path: file.clone() });

        let listed = sftp.list(&conn, &root).expect("list the scratch directory");
        let hello = listed
            .iter()
            .find(|e| e.name == "hello.txt")
            .unwrap_or_else(|| panic!("hello.txt is missing from {listed:?}"));
        assert_eq!(hello.kind, SftpEntryKind::File, "and it is a file");

        ok(SftpOp::Rename {
            from: file.clone(),
            to: moved.clone(),
        });
        let listed = sftp.list(&conn, &root).expect("list it again");
        assert!(
            listed.iter().any(|e| e.name == "moved.txt"),
            "the rename moved it: {listed:?}"
        );
        assert!(
            !listed.iter().any(|e| e.name == "hello.txt"),
            "and left nothing behind: {listed:?}"
        );

        // A path that is not there answers rather than hanging or succeeding.
        assert!(
            !matches!(
                sftp.op(
                    &conn,
                    &SftpOp::Stat {
                        path: format!("{root}/never-existed"),
                    }
                ),
                SftpOpResult::Done
            ),
            "stat of a missing path is not Done"
        );

        ok(SftpOp::RemoveFile { path: moved });
        ok(SftpOp::RemoveDir { path: root.clone() });
        assert!(
            sftp.list(&conn, &root).is_err(),
            "the scratch directory is gone"
        );

        conn.mark_dead();
        manager.evict_connection(conn.key());
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
