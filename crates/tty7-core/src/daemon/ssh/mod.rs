//! Native SSH session engine (Workstream 2).
//!
//! A single [`SshManager`] owns one tokio runtime and the registry of live
//! [`SshConnection`]s. The rest of the daemon is std-threads and never enters this
//! runtime; a native-SSH pane crosses the boundary only through the blocking
//! `Read`/`Write` adapters in [`session`] (fed by the async channel driver) and
//! the [`PromptBroker`] (auth/host-key round-trips).
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
/// Workspace-scoped control requests — see `workspace::handle`.
pub mod workspace;

mod auth;
mod connect;
mod handler;

/// A child process's stdio as one duplex stream. Re-exported (rather than
/// opening `connect` as a whole) because `daemon::remote_link` wraps a
/// `tty7-server --stdio` child in exactly the shape the `ProxyCommand` path
/// already uses.
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

/// Default connect+auth budget when the spec doesn't set one.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Identifies a reusable connection: same key ⇒ same authenticated transport.
/// Includes the full proxy configuration and (recursively) the jump chain, so two
/// specs that differ only in how they *reach* the host don't collide.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ConnectionKey(String);

impl ConnectionKey {
    /// The key as a string, for callers that need to *name* a connection —
    /// a log line, an error message, an installer's "which host am I writing
    /// to". Exposed because the alternative callers reach for is peeling the
    /// derived `Debug` output apart, which silently breaks the day anything
    /// about the formatting changes.
    ///
    /// It is a connection identity, not a display name: it carries the proxy
    /// and jump chain, and no user-facing label. Where the user has their own
    /// name for a host, prefer that and keep this for disambiguation.
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
    /// Memoized remote shell-integration probes, keyed like connections. A
    /// present `None` means "probed, nothing to inject" — cached just as firmly
    /// as a hit so an unintegrable host isn't re-probed on every new tab. See
    /// [`SshManager::remote_bootstrap`].
    probes: Mutex<HashMap<ConnectionKey, Option<(remote::RemoteShell, String)>>>,
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
                probes: Mutex::new(HashMap::new()),
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

        // Shell integration (OSC 133 + cwd reporting) for the remote shell. When
        // the remote is one we know how to bootstrap, the shell is started by an
        // `exec` request carrying a setup script that ends in `exec <shell>`,
        // rather than by a bare `shell` request; see `shell_integration::remote`.
        // Anything unrecognized — or a probe that couldn't be run — falls through
        // to the plain shell request, which is exactly what every session did
        // before this existed.
        // Opting out short-circuits the probe too, not just the bootstrap: a
        // profile with the switch off should cost nothing and touch nothing.
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

    // ---- Remote workspaces: one logical stream to a remote `tty7-server` ----

    /// Open one logical stream from this daemon to the `tty7-server` on `spec`'s
    /// host, reusing (or establishing) the machine's single authenticated
    /// connection.
    ///
    /// **One authentication per machine.** The connection comes from the same
    /// [`ConnectionKey`] registry the SSH panes use, so a workspace opened
    /// against a host the user already has a pane on costs no prompt at all, and
    /// a second workspace on the same host costs no second prompt — each stream
    /// is a new *channel*, never a new authentication. One channel
    /// per pane, one per workspace control stream; no multiplexing of our own on
    /// top of SSH's.
    ///
    /// The returned `Arc<SshConnection>` must be held for as long as the link is
    /// used: it is the last strong reference that keeps the shared connection
    /// (and therefore the channel) alive.
    /// **What `setup` buys.** Everything below this line may need a user: the
    /// authentication, the consent to write a binary onto the machine, the
    /// discovery that the daemon already there is a different build. `setup`
    /// carries the one client that can answer — see [`RouteSetup`].
    pub async fn open_remote_link(
        &self,
        spec: &NativeSshSpec,
        setup: &RouteSetup,
        server_command: Option<&str>,
    ) -> anyhow::Result<(RemoteLink, Arc<SshConnection>)> {
        let (conn, _reused) = self.open_connection(spec, &setup.broker).await?;

        // Before the first stream to a host, make sure the remote is actually
        // serving: the right version of `tty7-server`, installed and running.
        // Idempotent and cheap on the common path (two commands and one SFTP
        // stat, no download, no prompt), which is what makes it safe to call
        // before *every* link rather than once per connection.
        //
        // A `?` here means no link is opened at all, so "this machine has no
        // tty7-server" arrives as a route ack with a reason. B1 deliberately
        // left this un-stubbed rather than always-Ok for exactly that: an empty
        // implementation would turn a missing server into an opaque channel
        // failure much later.
        //
        // On a blocking thread because `Installer` is blocking start to finish
        // and one step of it waits on a human; running it on a runtime worker
        // would park the reactor that has to carry the answer back.
        let installed = {
            let install_conn = conn.clone();
            setup
                .blocking(move || crate::daemon::install::ensure_remote_server(&install_conn))
                .await??
        };

        // The installed binary's **absolute** path, not the bare name. Nothing
        // puts `~/.local/share/tty7/bin` on a non-interactive `PATH`, and the
        // file there is `tty7-server-c<control>p<protocol>` — so
        // `exec tty7-server --stdio` is a `command not found` on a machine the
        // install just succeeded on.
        // The install pass we just ran is what knows the path, so it hands it
        // over rather than leaving the transport to guess.
        let base = match server_command {
            Some(explicit) => explicit.to_string(),
            None => format!(
                "{} --stdio",
                crate::daemon::install::shell_quote(&installed)
            ),
        };
        let command = setup.channel.bridge_command(&base);

        // A pane connection never takes the cached `direct-streamlocal` entry:
        // that entry names the *control* socket, and the pane dialect is served
        // on a different one. See `RouteChannel::bridge_command`.
        let entry = match setup.channel {
            RouteChannel::Pane => RemoteEntry::SessionExec {
                command: command.clone(),
            },
            RouteChannel::Control => {
                conn.remote_entry_or_init(|| async {
                    let env = probe_remote_env(&conn).await;
                    let socket = env.as_ref().and_then(remote_link::remote_control_socket);
                    // Optimistic: `AllowStreamLocalForwarding` defaults to `yes`
                    // and the only way to learn otherwise is to be refused,
                    // which the demotion below turns into a permanent,
                    // connection-wide answer.
                    remote_link::choose_entry(socket.as_deref(), true, &command)
                })
                .await
            }
        };

        if let RemoteEntry::StreamLocal { socket } = &entry {
            match conn.open_direct_streamlocal(socket).await {
                Ok(channel) => return Ok((RemoteLink::stream_local(channel), conn)),
                Err(e) => {
                    // The refusal every later stream on this connection must not
                    // repeat: cache the fallback before taking it.
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

    /// Replace the `tty7-server` running on `spec`'s host with this client's
    /// build — "Restart Server", and **it drops every pane
    /// that server is hosting**.
    ///
    /// Only ever reached from a [`RouteAction::RestartServer`](crate::daemon::router::RouteAction)
    /// header, which a client only writes after a user has answered the
    /// keep-or-restart prompt with "Restart Server". Nothing in the connect path
    /// calls this: an older daemon on the far side keeps serving, because it owns
    /// live work and only its owner can decide to throw that away.
    ///
    /// Deliberately **not** an `ensure_remote_server` first. The mismatch that
    /// raises the prompt is discovered by an install pass that has already put
    /// this build's binary in place, so there is nothing left to install — and a
    /// second pass would rediscover the very mismatch the user is answering and
    /// relay a fresh prompt for it the moment the restart finished.
    pub async fn restart_remote_server(
        &self,
        spec: &NativeSshSpec,
        setup: &RouteSetup,
    ) -> anyhow::Result<()> {
        let (conn, _reused) = self.open_connection(spec, &setup.broker).await?;
        // Blocking start to finish (SIGTERM, poll for the socket to go, launch,
        // poll for it to answer) and it may stop to ask the user for a password
        // on the way in — the same reason `open_remote_link` keeps the installer
        // off the runtime's workers.
        setup
            .blocking(move || crate::daemon::install::restart_remote_daemon(&conn))
            .await??;
        Ok(())
    }

    /// Reinstall this client's `tty7-server` on `spec`'s host over whatever is at
    /// its path, then restart the daemon onto it — "Replace Server", and **it
    /// drops every pane that server is hosting**.
    ///
    /// Unlike [`restart_remote_server`](Self::restart_remote_server) this *does*
    /// write: it is the answer to a handshake that failed against a binary whose
    /// name promised a dialect it does not speak, so the file itself is what has
    /// to change. See [`crate::daemon::install::Installer::replace`].
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

    /// [`open_remote_link`](Self::open_remote_link) for the daemon's std threads
    /// (the router runs on one). Safe from any thread that is not itself a
    /// runtime worker — the server's connection threads never are.
    pub fn open_remote_link_blocking(
        &self,
        spec: &NativeSshSpec,
        setup: &RouteSetup,
        server_command: Option<&str>,
    ) -> anyhow::Result<(RemoteLink, Arc<SshConnection>)> {
        self.runtime
            .block_on(self.open_remote_link(spec, setup, server_command))
    }

    /// Drop a connection key's registry slot so the next `open_connection` for it
    /// establishes a fresh connection instead of upgrading a stale `Weak`. Called
    /// by the self-healing reuse path when a reused connection turns out dead.
    fn evict_connection(&self, key: &ConnectionKey) {
        self.conns.lock().unwrap().remove(key);
    }

    /// The shell-integration bootstrap script for `conn`'s next shell, or `None`
    /// to start that shell bare.
    ///
    /// Deciding costs one `exec` round-trip against the remote (see
    /// [`probe_remote_shell`]), so the answer is memoized on the connection key —
    /// the same identity connections are reused under. Opening a second tab to a
    /// host therefore pays nothing, and a *reconnect* to a host probed earlier
    /// pays nothing either: which shell a login lands in doesn't change between
    /// connections, so the cache deliberately outlives them.
    ///
    /// Two panes racing to a not-yet-probed host may both probe. That is a
    /// duplicated round-trip on a cold connection, not a correctness problem —
    /// the probe has no side effects and both arrive at the same answer — so it
    /// isn't worth serializing every spawn behind a per-key lock the way
    /// connection establishment is.
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

/// How long to wait for the shell probe before giving up on integrating a
/// remote. Generous, because the probe runs under the remote's login shell and
/// therefore behind whatever its `.zshenv` does; short enough that a host which
/// never answers costs a pause, not a hang. Expiring is not an error — the
/// session continues with a plain shell.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on probe output, in case the remote's startup files are chatty. Far more
/// than the two lines we asked for; a remote that exceeds it has already told us
/// everything [`remote::parse_probe`] could use.
const PROBE_OUTPUT_LIMIT: usize = 8 * 1024;

/// Ask the remote which login shell it would start, on a throwaway channel.
///
/// This is a non-PTY `exec`, so it runs and exits without touching the session
/// the user is about to get; nothing here can break that session, and every
/// failure path returns `None`, meaning "start the shell bare".
///
/// stderr is folded in with stdout because the marker-based parse tolerates
/// noise, and a remote whose startup files complain on stderr would otherwise
/// have its (perfectly good) answer thrown away.
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
    // A timeout doesn't discard what did arrive: the answer is on the second
    // line, so a remote that printed it and then stalled before closing the
    // channel is still perfectly readable.
    let _ = tokio::time::timeout(PROBE_TIMEOUT, collect).await;

    remote::parse_probe(&String::from_utf8_lossy(&out))
}

/// Read the four environment variables the remote's control socket path is
/// derived from, on a throwaway `exec` channel.
///
/// `None` when the remote said nothing usable — the caller then takes the
/// `--stdio` bridge, which resolves the path in the process that binds it, so a
/// failed probe costs a slower transport and never a failed connection.
///
/// stderr is folded in for the same reason the shell probe does it: the parse is
/// marker-based and tolerates noise, and discarding a good answer because the
/// remote's startup files complained would be gratuitous.
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

        // Identical connection params → identical key (reuse).
        assert_eq!(a, ConnectionKey::from_spec(&base_spec()));
    }

    /// `as_str` is what names a connection in prompts, logs and the installer's
    /// "which host am I writing to". It must carry the jump chain: two hosts
    /// reached through different bastions are different connections, and a
    /// label that collapsed them would put an install prompt on the wrong box.
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
        // Prove GSSAPI itself without requiring a GUI host-key prompt or mutating
        // the user's known_hosts from this live test.
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
