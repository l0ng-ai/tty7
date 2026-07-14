//! The authentication flow for a native SSH connection.
//!
//! Ordering follows the Tabby reference (brief §2): a leading `none` probe (which
//! also learns the server's remaining methods), then — for `Auto` — publickey,
//! agent, password, keyboard-interactive; a non-`Auto` mode restricts attempts to
//! that one family. The server's advertised remaining-methods set gates which
//! families are worth trying and is refreshed after each failure (only when the
//! server actually sends a non-empty set). Passwords/passphrases come from the
//! spec (pre-resolved from the keychain by the GUI) or, failing that, from the
//! [`PromptBroker`]. Secrets are never logged.

use std::sync::Arc;

use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{Algorithm, HashAlg, PrivateKeyWithHashAlg, PublicKey};
use russh::{MethodKind, MethodSet};

use crate::daemon::protocol::{AuthPromptKind, AuthResponse, KiPrompt, NativeSshSpec, SshAuthMode};

use super::broker::PromptBroker;
use super::handler::ClientHandler;

/// Attempt authentication. `Ok(())` = authenticated; `Err(reason)` carries a
/// user-facing reason for `SshStatus::Failed` (never a secret).
pub async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
) -> Result<(), String> {
    let user = spec.user.clone();

    // A `none` probe: some servers accept it, and either way it learns the
    // server's advertised remaining methods.
    let mut remaining = match handle
        .authenticate_none(&user)
        .await
        .map_err(|e| format!("auth (none) failed: {e}"))?
    {
        AuthResult::Success => return Ok(()),
        AuthResult::Failure {
            remaining_methods, ..
        } => remaining_methods,
    };

    let mut last_reason = "authentication failed".to_string();

    for family in method_order(spec.auth_mode) {
        // Respect the server's advertised set when it told us one: skip families
        // it won't accept. An empty set means "unknown" — try anyway.
        if !remaining.is_empty() && !remaining.contains(&family) {
            continue;
        }
        let outcome = match family {
            MethodKind::PublicKey => try_publickeys(handle, spec, broker).await,
            MethodKind::KeyboardInteractive => try_keyboard_interactive(handle, spec, broker).await,
            MethodKind::Password => try_password(handle, spec, broker).await,
            // Agent is folded into the publickey pass below via a distinct marker;
            // handled in `method_order` expansion.
            _ => Outcome::Skipped,
        };
        match outcome {
            Outcome::Authenticated => return Ok(()),
            Outcome::Failed {
                remaining_methods,
                reason,
            } => {
                if let Some(m) = remaining_methods
                    && !m.is_empty()
                {
                    remaining = m;
                }
                if let Some(r) = reason {
                    last_reason = r;
                }
            }
            Outcome::Skipped => {}
        }
    }

    Err(last_reason)
}

/// The ordered families to try for a given auth mode. `Agent` is represented as a
/// publickey attempt (it *is* publickey, signed by the agent), so it isn't a
/// separate `MethodKind`; `try_publickeys` covers both files and agent for `Auto`
/// and for the explicit `Agent`/`PublicKey` modes via `spec.auth_mode`.
fn method_order(mode: SshAuthMode) -> Vec<MethodKind> {
    match mode {
        SshAuthMode::Auto => vec![
            MethodKind::PublicKey,
            MethodKind::Password,
            MethodKind::KeyboardInteractive,
        ],
        SshAuthMode::PublicKey | SshAuthMode::Agent => vec![MethodKind::PublicKey],
        SshAuthMode::Password => vec![MethodKind::Password],
        SshAuthMode::KeyboardInteractive => vec![MethodKind::KeyboardInteractive],
    }
}

enum Outcome {
    Authenticated,
    Failed {
        remaining_methods: Option<MethodSet>,
        reason: Option<String>,
    },
    Skipped,
}

fn failed(reason: impl Into<String>) -> Outcome {
    Outcome::Failed {
        remaining_methods: None,
        reason: Some(reason.into()),
    }
}

/// Try identity files (unless mode is `Agent`) then the ssh-agent (unless mode is
/// `PublicKey`), in that order.
async fn try_publickeys(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
) -> Outcome {
    let mut last: Option<MethodSet> = None;

    if spec.auth_mode != SshAuthMode::Agent {
        for path in &spec.identity_files {
            match try_identity_file(handle, spec, broker, path).await {
                Outcome::Authenticated => return Outcome::Authenticated,
                Outcome::Failed {
                    remaining_methods, ..
                } => {
                    if remaining_methods.is_some() {
                        last = remaining_methods;
                    }
                }
                Outcome::Skipped => {}
            }
        }
    }

    if spec.auth_mode != SshAuthMode::PublicKey {
        match try_agent(handle, spec).await {
            Outcome::Authenticated => return Outcome::Authenticated,
            Outcome::Failed {
                remaining_methods, ..
            } => {
                if remaining_methods.is_some() {
                    last = remaining_methods;
                }
            }
            Outcome::Skipped => {}
        }
    }

    Outcome::Failed {
        remaining_methods: last,
        reason: Some("no public key was accepted".to_string()),
    }
}

async fn try_identity_file(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
    raw_path: &str,
) -> Outcome {
    let path = expand_identity_path(raw_path, &spec.host, &spec.user);
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return failed(format!("cannot read identity file {path}: {e}")),
    };

    // `.pub` misconfiguration: if the file parses as a *public* key, the user
    // pointed us at the public half. Skip it with a warning rather than fail.
    if PublicKey::from_openssh(contents.trim()).is_ok() {
        log::warn!("identity file {path} is a public key; skipping");
        return Outcome::Skipped;
    }

    let key = match russh::keys::decode_secret_key(&contents, None) {
        Ok(k) => k,
        Err(russh::keys::Error::KeyIsEncrypted) => {
            // Prefer a GUI-provided passphrase (keyed by the path as listed), else
            // prompt for one.
            let provided = spec
                .key_passphrases
                .as_ref()
                .and_then(|m| m.get(raw_path))
                .cloned();
            let passphrase = match provided {
                Some(p) => p,
                None => {
                    let resp = broker
                        .prompt(AuthPromptKind::KeyPassphrase {
                            key_path: raw_path.to_string(),
                            comment: String::new(),
                        })
                        .await;
                    match resp {
                        AuthResponse::Secret(p) => p,
                        _ => return Outcome::Skipped,
                    }
                }
            };
            match russh::keys::decode_secret_key(&contents, Some(&passphrase)) {
                Ok(k) => k,
                Err(e) => {
                    log::warn!("could not decrypt identity file {path}: {e}");
                    return failed(format!("could not decrypt identity file {path}"));
                }
            }
        }
        Err(e) => {
            log::warn!("could not read identity file {path}: {e}");
            return failed(format!("could not read identity file {path}"));
        }
    };

    let hash_alg = rsa_hash_alg(&key.algorithm());
    let pk = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
    match handle.authenticate_publickey(&spec.user, pk).await {
        Ok(AuthResult::Success) => Outcome::Authenticated,
        Ok(AuthResult::Failure {
            remaining_methods, ..
        }) => Outcome::Failed {
            remaining_methods: Some(remaining_methods),
            reason: Some(format!("server rejected key {raw_path}")),
        },
        Err(e) => failed(format!("public-key auth error: {e}")),
    }
}

async fn try_agent(handle: &mut Handle<ClientHandler>, spec: &NativeSshSpec) -> Outcome {
    // Agent transport is per-platform: a Unix-domain socket named by
    // SSH_AUTH_SOCK, or Windows OpenSSH's named pipe. The identity loop below
    // is shared via `try_agent_identities`, generic over the stream.
    #[cfg(unix)]
    {
        let agent = match AgentClient::connect_env().await {
            Ok(a) => a,
            // No agent available (SSH_AUTH_SOCK unset / unreachable): just skip.
            Err(_) => return Outcome::Skipped,
        };
        try_agent_identities(handle, spec, agent).await
    }
    #[cfg(windows)]
    {
        // Windows OpenSSH's agent listens on a fixed named pipe; honor
        // SSH_AUTH_SOCK as an override for nonstandard setups. (A Cygwin/MSYS
        // socket *file* in that variable simply fails to open → skip.)
        let pipe = std::env::var("SSH_AUTH_SOCK")
            .unwrap_or_else(|_| r"\\.\pipe\openssh-ssh-agent".to_string());
        let agent = match AgentClient::connect_named_pipe(&pipe).await {
            Ok(a) => a,
            // No agent available: just skip.
            Err(_) => return Outcome::Skipped,
        };
        try_agent_identities(handle, spec, agent).await
    }
}

async fn try_agent_identities<S>(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    mut agent: AgentClient<S>,
) -> Outcome
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    let identities = match agent.request_identities().await {
        Ok(ids) => ids,
        Err(_) => return Outcome::Skipped,
    };
    let mut last: Option<MethodSet> = None;
    for identity in identities {
        let pubkey: PublicKey = match &identity {
            AgentIdentity::PublicKey { key, .. } => key.clone(),
            // Certificate identities aren't handled in v1's agent path.
            AgentIdentity::Certificate { .. } => continue,
        };
        let hash_alg = rsa_hash_alg(&pubkey.algorithm());
        match handle
            .authenticate_publickey_with(&spec.user, pubkey, hash_alg, &mut agent)
            .await
        {
            Ok(AuthResult::Success) => return Outcome::Authenticated,
            Ok(AuthResult::Failure {
                remaining_methods, ..
            }) => last = Some(remaining_methods),
            // A signing error with this identity — try the next one.
            Err(_) => continue,
        }
    }
    Outcome::Failed {
        remaining_methods: last,
        reason: Some("no agent key was accepted".to_string()),
    }
}

async fn try_password(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
) -> Outcome {
    // Try a spec-provided (keychain-resolved) password first.
    if let Some(pw) = &spec.password {
        match handle.authenticate_password(&spec.user, pw.clone()).await {
            Ok(AuthResult::Success) => return Outcome::Authenticated,
            Ok(AuthResult::Failure { .. }) => {
                // The stored password was explicitly rejected (FR-A6): re-prompt.
                // The GUI can treat a fresh prompt after a provided password as
                // "stored password rejected" and offer to overwrite it.
            }
            Err(e) => return failed(format!("password auth error: {e}")),
        }
    }

    // Prompt the user (possibly after a rejected stored password).
    let resp = broker
        .prompt(AuthPromptKind::Password {
            user: spec.user.clone(),
            host: spec.host.clone(),
        })
        .await;
    let pw = match resp {
        AuthResponse::Secret(p) => p,
        _ => return failed("password entry cancelled"),
    };
    match handle.authenticate_password(&spec.user, pw).await {
        Ok(AuthResult::Success) => Outcome::Authenticated,
        Ok(AuthResult::Failure {
            remaining_methods, ..
        }) => Outcome::Failed {
            remaining_methods: Some(remaining_methods),
            reason: Some("password rejected".to_string()),
        },
        Err(e) => failed(format!("password auth error: {e}")),
    }
}

async fn try_keyboard_interactive(
    handle: &mut Handle<ClientHandler>,
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
) -> Outcome {
    let mut resp = match handle
        .authenticate_keyboard_interactive_start(&spec.user, None)
        .await
    {
        Ok(r) => r,
        Err(e) => return failed(format!("keyboard-interactive start error: {e}")),
    };

    // Cap the round count (OpenSSH keeps a similar client-side device cap): a
    // hostile or looping server must not be able to spin this task forever with
    // zero-prompt or auto-filled requests. The stored password is auto-filled
    // once only — a server re-asking means it was rejected (PAM retries), so
    // later rounds fall through to prompting the user for the real one.
    const MAX_ROUNDS: u32 = 16;
    let mut rounds = 0u32;
    let mut stored_password_used = false;
    loop {
        rounds += 1;
        if rounds > MAX_ROUNDS {
            return failed("keyboard-interactive gave up after too many rounds");
        }
        match resp {
            KeyboardInteractiveAuthResponse::Success => return Outcome::Authenticated,
            KeyboardInteractiveAuthResponse::Failure {
                remaining_methods, ..
            } => {
                return Outcome::Failed {
                    remaining_methods: Some(remaining_methods),
                    reason: Some("keyboard-interactive rejected".to_string()),
                };
            }
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                // Zero-prompt request (OpenSSH quirk): reply with an empty answer.
                if prompts.is_empty() {
                    resp = match handle
                        .authenticate_keyboard_interactive_respond(Vec::new())
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => return failed(format!("keyboard-interactive error: {e}")),
                    };
                    continue;
                }

                let allow_stored = !stored_password_used;
                stored_password_used = true;
                let answers = match collect_ki_answers(
                    spec,
                    broker,
                    &name,
                    &instructions,
                    &prompts,
                    allow_stored,
                )
                .await
                {
                    Some(a) => a,
                    None => return failed("keyboard-interactive cancelled"),
                };
                resp = match handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => return failed(format!("keyboard-interactive error: {e}")),
                };
            }
        }
    }
}

/// Answer a keyboard-interactive info-request. When *every* prompt is a
/// password-type field and a spec password is available (and this round may
/// still use it — the first only; a re-ask means the server rejected it),
/// auto-fill without bothering the GUI; otherwise surface the whole prompt set
/// to the GUI.
async fn collect_ki_answers(
    spec: &NativeSshSpec,
    broker: &Arc<PromptBroker>,
    name: &str,
    instructions: &str,
    prompts: &[russh::client::Prompt],
    allow_stored: bool,
) -> Option<Vec<String>> {
    let all_password_type = prompts
        .iter()
        .all(|p| !p.echo && p.prompt.to_lowercase().contains("password"));
    if all_password_type && allow_stored {
        if let Some(pw) = &spec.password {
            return Some(prompts.iter().map(|_| pw.clone()).collect());
        }
    }

    let ki_prompts: Vec<KiPrompt> = prompts
        .iter()
        .map(|p| KiPrompt {
            text: p.prompt.clone(),
            echo: p.echo,
        })
        .collect();
    let resp = broker
        .prompt(AuthPromptKind::KeyboardInteractive {
            name: name.to_string(),
            instructions: instructions.to_string(),
            prompts: ki_prompts,
        })
        .await;
    match resp {
        AuthResponse::Secrets(v) if v.len() == prompts.len() => Some(v),
        // A single-secret reply to a single prompt is also accepted.
        AuthResponse::Secret(s) if prompts.len() == 1 => Some(vec![s]),
        _ => None,
    }
}

/// RSA keys must be offered with a modern signature hash; russh maps `None` to
/// legacy SHA-1 for RSA, so pick SHA-256. For all other key types `hash_alg` is
/// ignored, so `None` is correct.
fn rsa_hash_alg(algorithm: &Algorithm) -> Option<HashAlg> {
    if matches!(algorithm, Algorithm::Rsa { .. }) {
        Some(HashAlg::Sha256)
    } else {
        None
    }
}

/// Expand an identity-file path: `%h`→host, `%r`→user, and a leading `~/` → home.
fn expand_identity_path(path: &str, host: &str, user: &str) -> String {
    let substituted = path.replace("%h", host).replace("%r", user);
    if let Some(rest) = substituted.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return format!("{home}/{rest}");
        }
    }
    substituted
}

#[cfg(unix)]
fn home_dir() -> Option<String> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty())
}

#[cfg(not(unix))]
fn home_dir() -> Option<String> {
    std::env::var("USERPROFILE").ok().filter(|h| !h.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_path_expands_tokens_and_tilde() {
        // Tokens expand regardless of home resolution.
        let p = expand_identity_path("/keys/%r@%h/id", "example.com", "deploy");
        assert_eq!(p, "/keys/deploy@example.com/id");
    }

    #[test]
    fn method_order_restricts_by_mode() {
        assert_eq!(
            method_order(SshAuthMode::Password),
            vec![MethodKind::Password]
        );
        assert_eq!(
            method_order(SshAuthMode::KeyboardInteractive),
            vec![MethodKind::KeyboardInteractive]
        );
        assert_eq!(
            method_order(SshAuthMode::Auto),
            vec![
                MethodKind::PublicKey,
                MethodKind::Password,
                MethodKind::KeyboardInteractive
            ]
        );
    }

    #[test]
    fn rsa_gets_sha256_others_none() {
        assert_eq!(
            rsa_hash_alg(&Algorithm::Rsa { hash: None }),
            Some(HashAlg::Sha256)
        );
        assert_eq!(rsa_hash_alg(&Algorithm::Ed25519), None);
    }
}
