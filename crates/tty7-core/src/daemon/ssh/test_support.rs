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
