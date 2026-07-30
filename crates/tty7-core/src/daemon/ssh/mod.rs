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
    LoopbackForward, LoopbackForwardId, LoopbackForwardInfo, ManagedForward, NativeSshSpec,
    SshForwardRule, SshPhase, WinSize,
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

    pub fn list_loopback_forwards(&self) -> Vec<LoopbackForwardInfo> {
        Vec::new()
    }

    pub fn close_loopback_forward(&self, _id: &LoopbackForwardId) -> bool {
        false
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
                self.probes.lock().unwrap().insert(key, probed.clone());
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
                    return Ok((conn, true));
                }
            }

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
            *guard = Arc::downgrade(&conn);
            Ok((conn, false))
        })
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

const PROBE_OUTPUT_LIMIT: usize = 8 * 1024;

async fn probe_remote_shell(conn: &SshConnection) -> Option<(remote::RemoteShell, String)> {
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

    remote::parse_probe(&String::from_utf8_lossy(&out))
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
}
