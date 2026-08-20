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

/// What a host-key status calls for, before anything is asked or written.
///
/// Split out of `check_server_key` so the policy can be read and tested on its
/// own: deciding whether to trust a key needs no server, no broker and no
/// known_hosts file, and only the carrying-out does.
#[derive(Debug)]
pub(super) enum HostKeyAction {
    Accept,
    Reject,
    Ask(Box<AuthPromptKind>),
}

/// The whole host-key policy table.
///
/// `verify_host_keys = false` still rejects a revoked key: turning verification
/// off says "I do not know this host and do not care", not "ignore a key its
/// owner has published as compromised".
pub(super) fn host_key_action(
    status: HostKeyStatus,
    verify_host_keys: bool,
    host: &str,
    port: u16,
    algorithm: String,
    fingerprint_sha256: String,
) -> HostKeyAction {
    if !verify_host_keys {
        return match status {
            HostKeyStatus::Revoked => HostKeyAction::Reject,
            _ => HostKeyAction::Accept,
        };
    }
    match status {
        HostKeyStatus::Known => HostKeyAction::Accept,
        HostKeyStatus::Revoked => HostKeyAction::Reject,
        HostKeyStatus::Unknown => HostKeyAction::Ask(Box::new(AuthPromptKind::HostKeyUnknown {
            host: host.to_string(),
            port,
            algorithm,
            fingerprint_sha256,
            previously_known_as: None,
        })),
        // Deliberately the unknown-host prompt and not a variant of its own:
        // `AuthPromptKind` crosses to the GUI *and* to whatever `tty7-server`
        // the far end happens to be running, and a new externally-tagged
        // variant is a hard decode failure on any peer that predates it. The
        // extra field is additive in both directions.
        HostKeyStatus::ChangedAlgorithm {
            known_algorithm, ..
        } => HostKeyAction::Ask(Box::new(AuthPromptKind::HostKeyUnknown {
            host: host.to_string(),
            port,
            algorithm,
            fingerprint_sha256,
            previously_known_as: Some(known_algorithm),
        })),
        HostKeyStatus::Changed {
            old_fingerprint_sha256,
        } => HostKeyAction::Ask(Box::new(AuthPromptKind::HostKeyChanged {
            host: host.to_string(),
            port,
            algorithm,
            fingerprint_sha256,
            old_fingerprint_sha256,
        })),
    }
}

/// Whether a prompt response accepts the key, and whether it asked for the key
/// to be written to known_hosts.
///
/// Rejecting never records: a `remember` alongside `accept: false` is the
/// dialog's checkbox state, not a request to trust the key.
pub(super) fn accepted_and_remembered(resp: &AuthResponse) -> (bool, bool) {
    match resp {
        AuthResponse::HostKeyDecision { accept, remember } => (*accept, *accept && *remember),
        _ => (false, false),
    }
}

impl ClientHandler {
    fn apply_decision(&self, resp: AuthResponse, key: &PublicKey) -> bool {
        let (accept, remember) = accepted_and_remembered(&resp);
        if !accept {
            return false;
        }
        if remember {
            // The superseded line has to go before the new one lands.
            // `known_hosts::check` answers `Known` on any same-algorithm match,
            // so an override that only appended left the key the user had just
            // rejected trusted for good. If it cannot be dropped, do not append
            // either: being asked again next time is the better half of that
            // trade.
            match known_hosts::forget_superseded(&self.host, self.port, key) {
                Ok(()) => {
                    if let Err(e) = known_hosts::append_trusted(&self.host, self.port, key) {
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
}

impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let status = known_hosts::check(&self.host, self.port, server_public_key);
        if !self.verify_host_keys && matches!(status, HostKeyStatus::Revoked) {
            log::warn!(
                "rejecting revoked host key for {}:{} despite verify_host_keys=false",
                self.host,
                self.port
            );
        }
        match host_key_action(
            status,
            self.verify_host_keys,
            &self.host,
            self.port,
            server_public_key.algorithm().as_str().to_string(),
            known_hosts::fingerprint_sha256(server_public_key),
        ) {
            HostKeyAction::Accept => Ok(true),
            HostKeyAction::Reject => Ok(false),
            HostKeyAction::Ask(prompt) => {
                let resp = self.broker.prompt(*prompt).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn action(status: HostKeyStatus, verify: bool) -> HostKeyAction {
        host_key_action(
            status,
            verify,
            "example.com",
            2222,
            "ssh-ed25519".to_string(),
            "SHA256:new".to_string(),
        )
    }

    #[test]
    fn a_key_already_on_file_is_accepted_without_asking() {
        assert!(matches!(
            action(HostKeyStatus::Known, true),
            HostKeyAction::Accept
        ));
    }

    #[test]
    fn a_revoked_key_is_rejected_even_with_verification_off() {
        assert!(matches!(
            action(HostKeyStatus::Revoked, true),
            HostKeyAction::Reject
        ));
        // The point of the whole arm: `verify_host_keys = false` means "I do
        // not know this host", not "ignore a key its owner has published as
        // compromised".
        assert!(matches!(
            action(HostKeyStatus::Revoked, false),
            HostKeyAction::Reject
        ));
    }

    #[test]
    fn verification_off_accepts_everything_else_without_asking() {
        for status in [
            HostKeyStatus::Known,
            HostKeyStatus::Unknown,
            HostKeyStatus::Changed {
                old_fingerprint_sha256: "SHA256:old".into(),
            },
            HostKeyStatus::ChangedAlgorithm {
                known_fingerprint_sha256: "SHA256:old".into(),
                known_algorithm: "ssh-rsa".into(),
            },
        ] {
            assert!(
                matches!(action(status.clone(), false), HostKeyAction::Accept),
                "{status:?} should be accepted outright with verification off"
            );
        }
    }

    #[test]
    fn an_unknown_host_is_asked_about_with_no_prior_algorithm() {
        let HostKeyAction::Ask(prompt) = action(HostKeyStatus::Unknown, true) else {
            panic!("an unknown host has to be asked about");
        };
        let AuthPromptKind::HostKeyUnknown {
            host,
            port,
            algorithm,
            fingerprint_sha256,
            previously_known_as,
        } = *prompt
        else {
            panic!("an unknown host gets the unknown-host prompt");
        };
        assert_eq!(host, "example.com");
        assert_eq!(port, 2222);
        assert_eq!(algorithm, "ssh-ed25519");
        assert_eq!(fingerprint_sha256, "SHA256:new");
        assert_eq!(previously_known_as, None);
    }

    /// A host that grows an ed25519 key beside its ssh-rsa one has not been
    /// tampered with, so it gets the *unknown* prompt rather than the
    /// man-in-the-middle one — carrying the old algorithm so the dialog can
    /// name what the host was known by. Sending a new prompt variant instead
    /// would be a hard decode failure on any older peer.
    #[test]
    fn a_new_algorithm_asks_the_unknown_prompt_naming_the_old_one() {
        let HostKeyAction::Ask(prompt) = action(
            HostKeyStatus::ChangedAlgorithm {
                known_fingerprint_sha256: "SHA256:old".into(),
                known_algorithm: "ssh-rsa".into(),
            },
            true,
        ) else {
            panic!("a new algorithm has to be asked about");
        };
        let AuthPromptKind::HostKeyUnknown {
            previously_known_as,
            ..
        } = *prompt
        else {
            panic!("a new algorithm must not use the changed-key prompt");
        };
        assert_eq!(previously_known_as.as_deref(), Some("ssh-rsa"));
    }

    /// A key that contradicts one on file under the same algorithm is the
    /// man-in-the-middle case, and gets the louder prompt with both
    /// fingerprints.
    #[test]
    fn a_contradicting_key_asks_the_changed_prompt_with_both_fingerprints() {
        let HostKeyAction::Ask(prompt) = action(
            HostKeyStatus::Changed {
                old_fingerprint_sha256: "SHA256:old".into(),
            },
            true,
        ) else {
            panic!("a changed key has to be asked about");
        };
        let AuthPromptKind::HostKeyChanged {
            fingerprint_sha256,
            old_fingerprint_sha256,
            ..
        } = *prompt
        else {
            panic!("a changed key must use the changed-key prompt");
        };
        assert_eq!(fingerprint_sha256, "SHA256:new");
        assert_eq!(old_fingerprint_sha256, "SHA256:old");
    }

    #[test]
    fn only_an_accepting_response_trusts_the_key() {
        assert_eq!(
            accepted_and_remembered(&AuthResponse::HostKeyDecision {
                accept: true,
                remember: false
            }),
            (true, false)
        );
        assert_eq!(
            accepted_and_remembered(&AuthResponse::HostKeyDecision {
                accept: true,
                remember: true
            }),
            (true, true)
        );
        assert_eq!(
            accepted_and_remembered(&AuthResponse::Cancelled),
            (false, false)
        );
        assert_eq!(
            accepted_and_remembered(&AuthResponse::Secret("hunter2".into())),
            (false, false),
            "a secret is not an answer to a host-key question"
        );
    }

    /// Rejecting never writes to known_hosts, whatever the checkbox said.
    #[test]
    fn a_rejection_never_records_the_key() {
        assert_eq!(
            accepted_and_remembered(&AuthResponse::HostKeyDecision {
                accept: false,
                remember: true
            }),
            (false, false)
        );
    }
}
