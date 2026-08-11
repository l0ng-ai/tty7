use std::sync::Arc;

use russh::Channel;
use russh::client::{ChannelOpenHandle, Msg, Session};
use russh::keys::PublicKey;
use tokio::net::TcpStream;

use crate::daemon::protocol::{AuthPromptKind, AuthResponse};

use super::broker::PromptBroker;
use super::forward::{self, RemoteForwardTable};
use super::known_hosts::{self, HostKeyStatus};

pub struct ClientHandler {
    pub host: String,
    pub port: u16,
    pub verify_host_keys: bool,
    pub skip_banner: bool,
    pub broker: Arc<PromptBroker>,
    pub remote_forwards: RemoteForwardTable,
}

impl ClientHandler {
    fn apply_decision(&self, resp: AuthResponse, key: &PublicKey) -> bool {
        match resp {
            AuthResponse::HostKeyDecision {
                accept: true,
                remember,
            } => {
                if remember {
                    // The superseded line has to go before the new one lands.
                    // `known_hosts::check` answers `Known` on any
                    // same-algorithm match, so an override that only appended
                    // left the key the user had just rejected trusted for good.
                    // If it cannot be dropped, do not append either: being
                    // asked again next time is the better half of that trade.
                    match known_hosts::forget_superseded(&self.host, self.port, key) {
                        Ok(()) => {
                            if let Err(e) = known_hosts::append_trusted(&self.host, self.port, key)
                            {
                                log::warn!("failed to record host key in known_hosts: {e}");
                            }
                        }
                        Err(e) => log::warn!(
                            "not recording host key: the superseded known_hosts line could not be removed: {e}"
                        ),
                    }
                }
                true
            }
            _ => false,
        }
    }
}

impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        if !self.verify_host_keys {
            let revoked = matches!(
                known_hosts::check(&self.host, self.port, server_public_key),
                HostKeyStatus::Revoked
            );
            if revoked {
                log::warn!(
                    "rejecting revoked host key for {}:{} despite verify_host_keys=false",
                    self.host,
                    self.port
                );
            }
            return Ok(!revoked);
        }

        let algorithm = server_public_key.algorithm().as_str().to_string();
        let fingerprint_sha256 = known_hosts::fingerprint_sha256(server_public_key);

        match known_hosts::check(&self.host, self.port, server_public_key) {
            HostKeyStatus::Known => Ok(true),
            HostKeyStatus::Revoked => Ok(false),
            HostKeyStatus::Unknown => {
                let resp = self
                    .broker
                    .prompt(AuthPromptKind::HostKeyUnknown {
                        host: self.host.clone(),
                        port: self.port,
                        algorithm,
                        fingerprint_sha256,
                        previously_known_as: None,
                    })
                    .await;
                Ok(self.apply_decision(resp, server_public_key))
            }
            // Deliberately the unknown-host prompt and not a variant of its
            // own: `AuthPromptKind` crosses to the GUI *and* to whatever
            // `tty7-server` the far end happens to be running, and a new
            // externally-tagged variant is a hard decode failure on any peer
            // that predates it. The extra field is additive in both
            // directions.
            HostKeyStatus::ChangedAlgorithm {
                known_algorithm, ..
            } => {
                let resp = self
                    .broker
                    .prompt(AuthPromptKind::HostKeyUnknown {
                        host: self.host.clone(),
                        port: self.port,
                        algorithm,
                        fingerprint_sha256,
                        previously_known_as: Some(known_algorithm),
                    })
                    .await;
                Ok(self.apply_decision(resp, server_public_key))
            }
            HostKeyStatus::Changed {
                old_fingerprint_sha256,
            } => {
                let resp = self
                    .broker
                    .prompt(AuthPromptKind::HostKeyChanged {
                        host: self.host.clone(),
                        port: self.port,
                        algorithm,
                        fingerprint_sha256,
                        old_fingerprint_sha256,
                    })
                    .await;
                Ok(self.apply_decision(resp, server_public_key))
            }
        }
    }

    async fn auth_banner(
        &mut self,
        banner: &str,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.skip_banner && !banner.is_empty() {
            self.broker.banner(banner.to_string());
        }
        Ok(())
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some((target_host, target_port)) = self
            .remote_forwards
            .lookup(connected_address, connected_port as u16)
        else {
            log::info!(
                "rejecting unmatched forwarded-tcpip channel on {connected_address}:{connected_port}"
            );
            return Ok(());
        };
        reply.accept().await;
        let stream = channel.into_stream();
        tokio::spawn(async move {
            match TcpStream::connect((target_host.as_str(), target_port)).await {
                Ok(sock) => {
                    let _ = forward::bridge(stream, sock).await;
                }
                Err(e) => log::info!(
                    "remote forward: local connect to {target_host}:{target_port} failed: {e}"
                ),
            }
        });
        Ok(())
    }
}
