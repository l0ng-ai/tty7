//! The russh client [`Handler`]: host-key verification, auth banners, and
//! incoming forwarded channels.
//!
//! russh invokes `check_server_key` during the handshake (once per connection —
//! reused connections never re-run it) and `auth_banner` if the server sends one.
//! Both route through the [`PromptBroker`] so the *GUI* makes the trust decision
//! and sees the banner; the daemon owns the `known_hosts` storage per PRD §3.4.
//!
//! `server_channel_open_forwarded_tcpip` implements the Remote-forward
//! (`tcpip-forward`) receive side (WS4): incoming channels are matched against the
//! connection's [`RemoteForwardTable`] and bridged to a local socket.
//!
//! **X11 seam (P1, FR-X2 — deferred).** WS2 carries `NativeSshSpec.x11` but never
//! requests `x11-req` on the shell channel, so no X11 channels arrive and the
//! default `server_channel_open_x11` (auto-reject on drop) is correct. Wiring X11
//! would add: `channel.request_x11(..)` at shell start (with a MIT-MAGIC-COOKIE-1
//! cookie), a `server_channel_open_x11` override here that resolves the local
//! display (`$DISPLAY` → `/tmp/.X11-unix/X<n>` unix socket or `localhost:6000+n`),
//! and `forward::bridge` to that socket — mirroring the forwarded-tcpip path below.
//! Left unimplemented deliberately (macOS needs XQuartz; low priority).

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
    /// The connection's Remote-forward bindings (WS4). Shared with its
    /// [`super::session::SshConnection`]; incoming `forwarded-tcpip` channels are
    /// matched against it and bridged to the registered local target.
    pub remote_forwards: RemoteForwardTable,
}

impl ClientHandler {
    /// Turn a GUI host-key decision into an accept/reject, appending to
    /// `known_hosts` when the user chose to remember it. A remember-append failure
    /// is logged but does not veto the (already-granted) session — the user
    /// approved this key for this connection either way.
    fn apply_decision(&self, resp: AuthResponse, key: &PublicKey) -> bool {
        match resp {
            AuthResponse::HostKeyDecision {
                accept: true,
                remember,
            } => {
                if remember {
                    if let Err(e) = known_hosts::append_trusted(&self.host, self.port, key) {
                        log::warn!("failed to record host key in known_hosts: {e}");
                    }
                }
                true
            }
            // Explicit reject, a cancel, or a mismatched response kind: refuse.
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
        // A per-profile / global opt-out (FR-S4): trust without prompting — but
        // still honor `@revoked` markers, like OpenSSH under
        // `StrictHostKeyChecking no`: an explicitly revoked key is never
        // acceptable, opt-out or not.
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
            // A revoked key is a hard reject — never even offer to trust it.
            HostKeyStatus::Revoked => Ok(false),
            HostKeyStatus::Unknown => {
                let resp = self
                    .broker
                    .prompt(AuthPromptKind::HostKeyUnknown {
                        host: self.host.clone(),
                        port: self.port,
                        algorithm,
                        fingerprint_sha256,
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

    /// An incoming connection on a Remote (`tcpip-forward`) binding. Match it
    /// against this connection's registered forwards; on a hit, accept the channel
    /// and bridge it to a fresh local TCP connection to the target. An unmatched
    /// channel is rejected (dropping `reply` rejects) — a remote forward we don't
    /// own must not be tunneled anywhere.
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
            // Dropping `reply` rejects the channel.
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
