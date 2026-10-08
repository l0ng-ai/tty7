//! An SSH server in the test process, for what `FakeRemote` cannot see.
//!
//! The install layer's fake answers `RemoteOps` calls without a wire, so
//! whether the channels behind those calls are ever closed — what sshd's
//! `MaxSessions` counts — was outside every test. This server takes one
//! connection, accepts session channels up to a limit, answers `exec` the way
//! the test asks, and counts what the client opened and closed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use russh::keys::PrivateKey;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::server::{self, Auth, ChannelOpenHandle, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure};

use crate::daemon::protocol::{NativeSshSpec, SshAuthMode, SshProxy};

use super::broker::PromptBroker;
use super::forward::RemoteForwardTable;
use super::handler::ClientHandler;
use super::{ConnectionKey, SshConnection};

/// How the server answers an `exec` request.
#[derive(Clone, Copy)]
pub(crate) enum Exec {
    /// Success, a line of output, exit status 0, EOF and CLOSE: a command
    /// that ran and finished, after which the server closes first.
    Exits,
    /// Success and nothing more: a command that never finishes. The server
    /// never closes, so only the client can.
    Hangs,
}

#[derive(Default)]
struct Counts {
    opened: AtomicUsize,
    closed: AtomicUsize,
    refused: AtomicUsize,
}

struct Sshd {
    exec: Exec,
    max_sessions: Option<usize>,
    counts: Arc<Counts>,
    /// `Some` for a server that has stopped answering: each open is held
    /// here, neither accepted nor refused, the way a request sits when its
    /// reply can no longer get back.
    unanswered: Option<Vec<ChannelOpenHandle>>,
}

impl server::Handler for Sshd {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<server::Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(held) = self.unanswered.as_mut() {
            held.push(reply);
            return Ok(());
        }
        let open = self
            .counts
            .opened
            .load(Ordering::SeqCst)
            .saturating_sub(self.counts.closed.load(Ordering::SeqCst));
        if self.max_sessions.is_some_and(|max| open >= max) {
            self.counts.refused.fetch_add(1, Ordering::SeqCst);
            // sshd's answer once MaxSessions are all taken.
            reply.reject(ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        }
        self.counts.opened.fetch_add(1, Ordering::SeqCst);
        reply.accept().await;
        Ok(())
    }

    /// Held the same way when the server has stopped answering — which, from
    /// the client's side, is indistinguishable from a server still waiting
    /// on its own connect to a target that drops SYNs.
    async fn channel_open_direct_tcpip(
        &mut self,
        _channel: Channel<server::Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(held) = self.unanswered.as_mut() {
            held.push(reply);
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _command: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        if let Exec::Exits = self.exec {
            session.data(channel, &b"ok\n"[..])?;
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
        }
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, &b"$ "[..])?;
        Ok(())
    }

    async fn channel_close(
        &mut self,
        _channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.counts.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// One connection to a server in this process, and what the server counted.
pub(crate) struct FakeSshd {
    pub(crate) conn: Arc<SshConnection>,
    counts: Arc<Counts>,
}

impl FakeSshd {
    pub(crate) async fn connect(exec: Exec, max_sessions: Option<usize>) -> FakeSshd {
        Self::start(exec, max_sessions, false).await
    }

    /// A server that completes the handshake and then never answers a channel
    /// open: the connection stays up and nothing on it errors.
    pub(crate) async fn connect_unanswering() -> FakeSshd {
        Self::start(Exec::Hangs, None, true).await
    }

    async fn start(exec: Exec, max_sessions: Option<usize>, unanswering: bool) -> FakeSshd {
        let counts = Arc::new(Counts::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("bound address");

        let mut config = server::Config::default();
        config.inactivity_timeout = None;
        config
            .keys
            .push(PrivateKey::from(Ed25519Keypair::from_seed(&[7; 32])));
        let handler = Sshd {
            exec,
            max_sessions,
            counts: Arc::clone(&counts),
            unanswered: unanswering.then(Vec::new),
        };
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept the test client");
            let running = server::run_stream(Arc::new(config), socket, handler)
                .await
                .expect("server handshake");
            let _ = running.await;
        });

        let spec = spec_for(addr.port());
        let remote_forwards = RemoteForwardTable::default();
        let handler = ClientHandler {
            host: spec.host.clone(),
            port: spec.port,
            verify_host_keys: false,
            skip_banner: true,
            broker: PromptBroker::new(Box::new(|_| true)),
            remote_forwards: remote_forwards.clone(),
        };
        let mut handle =
            russh::client::connect(Arc::new(russh::client::Config::default()), addr, handler)
                .await
                .expect("client handshake");
        let auth = handle
            .authenticate_none("tester")
            .await
            .expect("auth round trip");
        assert!(auth.success(), "the fake accepts everyone");
        let conn = SshConnection::new(handle, ConnectionKey::from_spec(&spec), remote_forwards);
        FakeSshd { conn, counts }
    }

    /// Session channels the server accepted.
    pub(crate) fn opened(&self) -> usize {
        self.counts.opened.load(Ordering::SeqCst)
    }

    /// CHANNEL_CLOSEs the server received.
    pub(crate) fn closed(&self) -> usize {
        self.counts.closed.load(Ordering::SeqCst)
    }

    /// Session opens the server refused at its limit.
    pub(crate) fn refused(&self) -> usize {
        self.counts.refused.load(Ordering::SeqCst)
    }

    /// Waits for the server to have received `n` closes. A close sent on
    /// drop is queued for the client's session task, so it reaches the server
    /// a moment after the holder is gone, never before.
    pub(crate) async fn wait_for_closed(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.closed() < n {
            assert!(
                Instant::now() < deadline,
                "the server received {} of {n} closes",
                self.closed()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

pub(crate) fn base_spec() -> NativeSshSpec {
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
        remote_clipboard_write: false,
        login_script: vec![],
        display_name: None,
        profile_id: None,
    }
}

fn spec_for(port: u16) -> NativeSshSpec {
    let mut spec = base_spec();
    spec.host = "127.0.0.1".into();
    spec.port = port;
    spec.verify_host_keys = false;
    spec
}

/// What a password server was asked for, so a test can say not only what the
/// user was shown but what actually went out on the wire.
#[derive(Default)]
struct AuthCounts {
    passwords: AtomicUsize,
    kbdint: AtomicUsize,
}

/// A server that wants a password and offers `keyboard-interactive` as the
/// other way to hand it one, counting both.
///
/// This is the shape of the host in #820: OpenSSH with `PasswordAuthentication
/// yes` advertises both methods, and a client that treats them as two separate
/// questions asks the same person the same thing twice.
struct PasswordSshd {
    secret: String,
    counts: Arc<AuthCounts>,
}

impl PasswordSshd {
    fn reject() -> Auth {
        Auth::Reject {
            proceed_with_methods: Some(russh::MethodSet::from(
                &[
                    russh::MethodKind::Password,
                    russh::MethodKind::KeyboardInteractive,
                ][..],
            )),
            partial_success: false,
        }
    }
}

impl server::Handler for PasswordSshd {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Self::reject())
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.counts.passwords.fetch_add(1, Ordering::SeqCst);
        match password == self.secret {
            true => Ok(Auth::Accept),
            false => Ok(Self::reject()),
        }
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _user: &str,
        _submethods: &str,
        response: Option<russh::server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        self.counts.kbdint.fetch_add(1, Ordering::SeqCst);
        match response {
            // The opening request asks the question; only a client that got an
            // answer out of somebody comes back carrying one.
            None => Ok(Auth::Partial {
                name: "".into(),
                instructions: "".into(),
                prompts: vec![("Password: ".into(), false)].into(),
            }),
            Some(_) => Ok(Self::reject()),
        }
    }
}

/// A client handle parked on a password server with not one authentication
/// method run yet, so a test can drive [`super::auth::authenticate`] itself.
pub(crate) struct PasswordFake {
    pub(crate) handle: russh::client::Handle<ClientHandler>,
    pub(crate) spec: NativeSshSpec,
    counts: Arc<AuthCounts>,
}

impl PasswordFake {
    pub(crate) async fn connect(secret: &str) -> PasswordFake {
        let counts = Arc::new(AuthCounts::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("bound address");

        let mut config = server::Config::default();
        config.inactivity_timeout = None;
        // Rejections are deliberately slow in the default config, and these
        // tests walk through several.
        config.auth_rejection_time = Duration::from_millis(0);
        config.auth_rejection_time_initial = Some(Duration::from_millis(0));
        config
            .keys
            .push(PrivateKey::from(Ed25519Keypair::from_seed(&[9; 32])));
        let handler = PasswordSshd {
            secret: secret.to_string(),
            counts: Arc::clone(&counts),
        };
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept the test client");
            let running = server::run_stream(Arc::new(config), socket, handler)
                .await
                .expect("server handshake");
            let _ = running.await;
        });

        let spec = spec_for(addr.port());
        let handler = ClientHandler {
            host: spec.host.clone(),
            port: spec.port,
            verify_host_keys: false,
            skip_banner: true,
            broker: PromptBroker::new(Box::new(|_| true)),
            remote_forwards: RemoteForwardTable::default(),
        };
        let handle =
            russh::client::connect(Arc::new(russh::client::Config::default()), addr, handler)
                .await
                .expect("client handshake");
        PasswordFake {
            handle,
            spec,
            counts,
        }
    }

    /// `userauth` requests the server saw, per method.
    pub(crate) fn password_attempts(&self) -> usize {
        self.counts.passwords.load(Ordering::SeqCst)
    }

    pub(crate) fn kbdint_attempts(&self) -> usize {
        self.counts.kbdint.load(Ordering::SeqCst)
    }
}
