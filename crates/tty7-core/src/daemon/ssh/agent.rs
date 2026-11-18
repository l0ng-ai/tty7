//! Connection-scoped OpenSSH agent forwarding. Agent streams carry raw bytes;
//! neither the remote host nor this bridge needs a copy of a private key.

use std::time::Duration;

use russh::{ChannelMsg, keys::agent::client::AgentClient};

use super::SshConnection;

pub fn local_endpoint() -> Option<String> {
    std::env::var("SSH_AUTH_SOCK")
        .ok()
        .or_else(|| cfg!(windows).then(|| r"\\.\pipe\openssh-ssh-agent".to_string()))
}

impl SshConnection {
    pub(super) async fn open_shell_channel(
        &self,
        spec: &crate::daemon::protocol::NativeSshSpec,
    ) -> Result<russh::Channel<russh::client::Msg>, russh::Error> {
        if spec.agent_forward {
            if let Err(e) = self.forwarded_agent_socket().await {
                log::warn!("SSH agent forwarding unavailable: {e}");
            }
        }
        self.open_session_channel().await
    }
    /// sshd accepts this request once per transport, even when many panes
    /// share it. Cache failures too: a timed-out request may have succeeded.
    pub async fn forwarded_agent_socket(&self) -> Result<String, String> {
        let mut cached = self.agent_socket.lock().await;
        if let Some(result) = cached.as_ref() {
            return result.clone();
        }
        *cached = Some(Err("agent forwarding initialization was interrupted".into()));
        let result = tokio::time::timeout(Duration::from_secs(10), self.probe_agent_socket())
            .await
            .unwrap_or_else(|_| Err("agent forwarding initialization timed out".into()));
        *cached = Some(result.clone());
        result
    }

    async fn probe_agent_socket(&self) -> Result<String, String> {
        let mut channel = self
            .open_command_channel()
            .await
            .map_err(|e| e.to_string())?;
        channel
            .agent_forward(true)
            .await
            .map_err(|e| e.to_string())?;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => break,
                Some(ChannelMsg::Failure) => {
                    return Err("the SSH server refused agent forwarding".into());
                }
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => {
                    return Err("the SSH server closed the agent request".into());
                }
                _ => {}
            }
        }
        // A nonce and NUL framing tolerate output from the remote shell's rc.
        // No tty7-server flag is needed, so this works with an older binary.
        let marker = format!("TTY7_AGENT_{}:", uuid::Uuid::new_v4().simple());
        let command = format!("printf '%s%s\\000' '{marker}' \"$SSH_AUTH_SOCK\"");
        channel
            .exec(true, command)
            .await
            .map_err(|e| e.to_string())?;
        let mut output = Vec::new();
        let mut status = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    if output.len() + data.len() > 64 * 1024 {
                        return Err("agent socket probe produced too much output".into());
                    }
                    output.extend_from_slice(&data);
                }
                ChannelMsg::Failure => {
                    return Err("the SSH server refused the agent socket probe".into());
                }
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        if status != Some(0) {
            return Err(
                "the remote shell could not report its agent socket (Linux/macOS required)".into(),
            );
        }
        parse_socket(&output, marker.as_bytes())
    }
}

fn parse_socket(output: &[u8], marker: &[u8]) -> Result<String, String> {
    let start = output
        .windows(marker.len())
        .position(|w| w == marker)
        .ok_or("the remote shell did not report an agent socket")?
        + marker.len();
    let tail = &output[start..];
    let end = tail
        .iter()
        .position(|b| *b == 0)
        .ok_or("incomplete agent socket probe")?;
    let socket = std::str::from_utf8(&tail[..end]).map_err(|_| "agent socket is not UTF-8")?;
    if !socket.starts_with('/') || socket.len() > 107 {
        return Err("the remote shell reported no usable Unix agent socket".into());
    }
    Ok(socket.to_string())
}

pub(crate) async fn connect(
    endpoint: &str,
) -> Result<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>, String> {
    if endpoint.is_empty() {
        return Err("no local SSH agent is available".into());
    }
    #[cfg(unix)]
    let agent = AgentClient::connect_uds(endpoint).await;
    #[cfg(windows)]
    let agent = AgentClient::connect_named_pipe(endpoint).await;
    agent.map(|a| a.into_inner()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_probe_tolerates_banner_and_preserves_spaces() {
        assert_eq!(
            parse_socket(b"welcome\nnonce:/tmp/my agent\0trailer", b"nonce:").unwrap(),
            "/tmp/my agent"
        );
        for output in [
            b"nonce:\0".as_slice(),
            b"nonce:relative\0",
            b"nonce:/truncated",
            b"noise",
        ] {
            assert!(parse_socket(output, b"nonce:").is_err());
        }
    }

    struct AgentSshd {
        requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        allow: bool,
        channels: std::collections::HashMap<russh::ChannelId, russh::Channel<russh::server::Msg>>,
    }

    impl russh::server::Handler for AgentSshd {
        type Error = russh::Error;

        async fn auth_none(&mut self, _: &str) -> Result<russh::server::Auth, Self::Error> {
            Ok(russh::server::Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            channel: russh::Channel<russh::server::Msg>,
            reply: russh::server::ChannelOpenHandle,
            _: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            self.channels.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        async fn agent_request(
            &mut self,
            channel: russh::ChannelId,
            session: &mut russh::server::Session,
        ) -> Result<bool, Self::Error> {
            let first = self
                .requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0;
            let allowed = self.allow && first;
            if allowed {
                session.channel_success(channel)?;
            } else {
                session.channel_failure(channel)?;
            }
            Ok(allowed)
        }

        async fn exec_request(
            &mut self,
            channel: russh::ChannelId,
            command: &[u8],
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            let text = std::str::from_utf8(command).unwrap();
            let marker = text.split('\'').nth(3).unwrap();
            session.channel_success(channel)?;
            session.data(
                channel,
                format!("banner\n{marker}/tmp/forwarded-agent\0").into_bytes(),
            )?;
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
            Ok(())
        }

        async fn channel_close(
            &mut self,
            channel: russh::ChannelId,
            _: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            self.channels.remove(&channel);
            Ok(())
        }
    }

    async fn ssh(
        endpoint: Option<String>,
        allow: bool,
    ) -> (
        std::sync::Arc<SshConnection>,
        russh::server::Handle,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use russh::keys::{PrivateKey, ssh_key::private::Ed25519Keypair};
        use std::sync::{Arc, atomic::AtomicUsize};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let handler = AgentSshd {
            requests: requests.clone(),
            allow,
            channels: Default::default(),
        };
        let mut config = russh::server::Config::default();
        config
            .keys
            .push(PrivateKey::from(Ed25519Keypair::from_seed(&[9; 32])));
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let running = russh::server::run_stream(Arc::new(config), socket, handler)
                .await
                .unwrap();
            let _ = tx.send(running.handle());
            let _ = running.await;
        });
        let forwards = super::super::forward::RemoteForwardTable::default();
        let handler = super::super::handler::ClientHandler {
            agent_endpoint: endpoint,
            agent_channels: Arc::new(tokio::sync::Semaphore::new(32)),
            host: "localhost".into(),
            port: address.port(),
            verify_host_keys: false,
            skip_banner: true,
            broker: super::super::PromptBroker::new(Box::new(|_| true)),
            remote_forwards: forwards.clone(),
        };
        let mut handle = russh::client::connect(Arc::new(Default::default()), address, handler)
            .await
            .unwrap();
        assert!(handle.authenticate_none("test").await.unwrap().success());
        let conn = SshConnection::new(
            handle,
            super::super::ConnectionKey::from_spec(&super::super::test_support::base_spec()),
            forwards,
        );
        (conn, rx.await.unwrap(), requests)
    }

    #[tokio::test]
    async fn initialization_is_serialized_and_server_refusal_is_cached() {
        let (conn, _, requests) = ssh(None, true).await;
        let (a, b) = tokio::join!(conn.forwarded_agent_socket(), conn.forwarded_agent_socket());
        assert_eq!(a.unwrap(), "/tmp/forwarded-agent");
        assert_eq!(b.unwrap(), "/tmp/forwarded-agent");
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (refused, _, requests) = ssh(None, false).await;
        assert!(
            refused
                .forwarded_agent_socket()
                .await
                .unwrap_err()
                .contains("refused")
        );
        assert!(refused.forwarded_agent_socket().await.is_err());
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forwarded_agent_lists_keys_and_returns_the_same_signature_as_local_agent() {
        use russh::keys::{PrivateKey, ssh_key::private::Ed25519Keypair};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let incoming = smol::stream::unfold(listener, |listener| async move {
            Some((listener.accept().await.map(|(socket, _)| socket), listener))
        });
        let server = tokio::spawn(russh::keys::agent::server::serve(Box::pin(incoming), ()));
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[13; 32]));
        let mut local = AgentClient::connect_uds(&path).await.unwrap();
        local.add_identity(&key, &[]).await.unwrap();
        let (conn, remote, _) = ssh(Some(path.to_string_lossy().into_owned()), true).await;
        conn.forwarded_agent_socket().await.unwrap();
        let channel = tokio::time::timeout(Duration::from_secs(3), remote.channel_open_agent())
            .await
            .unwrap()
            .unwrap();
        let mut forwarded = AgentClient::connect(channel.into_stream());
        let ids = forwarded.request_identities().await.unwrap();
        assert_eq!(ids.len(), 1);
        let data = b"tty7 forwarding signature test";
        let expected = local
            .sign_request_signature(key.public_key(), None, data)
            .await
            .unwrap();
        let actual = forwarded
            .sign_request_signature(key.public_key(), None, data)
            .await
            .unwrap();
        assert_eq!(actual, expected);
        let (_disabled, remote, _) = ssh(None, true).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(3), remote.channel_open_agent())
                .await
                .unwrap()
                .is_err()
        );
        server.abort();
    }
}
