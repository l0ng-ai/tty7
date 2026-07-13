//! The russh client [`Handler`]: host-key verification and auth banners.
//!
//! russh invokes `check_server_key` during the handshake (once per connection —
//! reused connections never re-run it) and `auth_banner` if the server sends one.
//! Both route through the [`PromptBroker`] so the *GUI* makes the trust decision
//! and sees the banner; the daemon owns the `known_hosts` storage per PRD §3.4.

use std::sync::Arc;

use russh::client::Session;
use russh::keys::PublicKey;

use crate::daemon::protocol::{AuthPromptKind, AuthResponse};

use super::broker::PromptBroker;
use super::known_hosts::{self, HostKeyStatus};

pub struct ClientHandler {
    pub host: String,
    pub port: u16,
    pub verify_host_keys: bool,
    pub skip_banner: bool,
    pub broker: Arc<PromptBroker>,
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

    async fn check_server_key(&mut self, server_public_key: &PublicKey) -> Result<bool, Self::Error> {
        // A per-profile / global opt-out (FR-S4): trust unconditionally.
        if !self.verify_host_keys {
            return Ok(true);
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

    async fn auth_banner(&mut self, banner: &str, _session: &mut Session) -> Result<(), Self::Error> {
        if !self.skip_banner && !banner.is_empty() {
            self.broker.banner(banner.to_string());
        }
        Ok(())
    }
}
