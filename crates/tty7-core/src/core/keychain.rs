//! The *naming* half of the SSH credential vault: how a keychain entry is
//! addressed, and the secret-free pointer that `config.json` persists.
//!
//! Secrets (passwords, private-key passphrases) live only in the platform secret
//! store — never in `config.json`. A profile persists at most a [`CredentialRef`],
//! which *names* a keychain entry but carries no secret. Per PRD §7.2 entries are
//! keyed by **endpoint**, not by profile:
//!
//! - passwords → service `tty7-ssh`, account `<user>@<host>:<port>`
//! - key passphrases → service `tty7-ssh-key`, account `<sha512-hex of key file>`
//!
//! Endpoint keying lets a QuickConnect (which has no profile) still "remember" a
//! password, lets several profiles pointing at one endpoint share one credential,
//! and means changing a password touches exactly one entry.
//!
//! **The store itself is not here.** The `CredentialStore` trait, its OS-keychain
//! backend and the in-memory test double live in the GUI crate
//! (`tty7::core::keychain`), because nothing in this crate reads or writes a
//! secret: the daemon receives already-resolved secrets on the wire (see
//! `daemon::protocol`'s `NativeSshSpec`) and the headless `tty7-server` runs on
//! boxes that have no OS keychain at all. Keeping `keyring` out of this crate's
//! manifest is what keeps a static `tty7-server` from linking the whole
//! `zbus`/`secret-service` stack it can never use.
//!
//! What has to stay is exactly what `Config` needs to parse `config.json`
//! identically on the server: the account-naming scheme and [`CredentialRef`].

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

/// Keychain service name for endpoint passwords.
pub const SERVICE_PASSWORD: &str = "tty7-ssh";
/// Keychain service name for private-key passphrases.
pub const SERVICE_KEY_PASSPHRASE: &str = "tty7-ssh-key";

/// Which kind of secret a [`CredentialRef`] points at. The kind selects the
/// keychain *service*; the ref's `account` selects the entry within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialKind {
    /// An endpoint password (`tty7-ssh` service, `user@host:port` account).
    #[default]
    Password,
    /// A private-key passphrase (`tty7-ssh-key` service, key-sha512-hex account).
    KeyPassphrase,
}

impl CredentialKind {
    /// The keychain service name this kind stores under.
    pub fn service(self) -> &'static str {
        match self {
            CredentialKind::Password => SERVICE_PASSWORD,
            CredentialKind::KeyPassphrase => SERVICE_KEY_PASSPHRASE,
        }
    }
}

/// A persisted, secret-free pointer to a keychain entry. This is the only
/// credential-related thing that ever lands in `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct CredentialRef {
    /// Whether this names a password or a key passphrase.
    #[serde(deserialize_with = "crate::core::config::de_lenient")]
    pub kind: CredentialKind,
    /// The keychain "account": `user@host:port` for [`CredentialKind::Password`],
    /// or the sha512-hex of the key-file contents for
    /// [`CredentialKind::KeyPassphrase`].
    pub account: String,
}

impl Default for CredentialRef {
    fn default() -> Self {
        Self {
            kind: CredentialKind::Password,
            account: String::new(),
        }
    }
}

impl CredentialRef {
    /// Reference the password entry for an endpoint.
    pub fn password(user: &str, host: &str, port: u16) -> Self {
        Self {
            kind: CredentialKind::Password,
            account: endpoint_account(user, host, port),
        }
    }

    /// Reference the passphrase entry for a private key, given the sha512-hex of
    /// its file contents (see [`key_account_from_contents`]).
    pub fn key_passphrase(key_sha512_hex: impl Into<String>) -> Self {
        Self {
            kind: CredentialKind::KeyPassphrase,
            account: key_sha512_hex.into(),
        }
    }

    /// The keychain service this ref resolves under.
    pub fn service(&self) -> &'static str {
        self.kind.service()
    }
}

/// The endpoint account string used to key a password entry: `user@host:port`.
pub fn endpoint_account(user: &str, host: &str, port: u16) -> String {
    format!("{user}@{host}:{port}")
}

/// The account string used to key a private-key passphrase entry: the lowercase
/// sha512-hex digest of the key file's raw contents. Endpoint-independent, so the
/// same encrypted key reused across hosts shares one stored passphrase.
///
/// Only the GUI calls this — it is the side that reads the key file — but the
/// account *name* is part of the persisted config contract, the same as
/// [`endpoint_account`], so both halves of PRD §7.2's keying scheme stay in one
/// place rather than drifting apart across the crate boundary.
pub fn key_account_from_contents(key_bytes: &[u8]) -> String {
    let digest = Sha512::digest(key_bytes);
    // Lowercase hex, no separators.
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_and_key_accounts_are_stable() {
        assert_eq!(
            endpoint_account("deploy", "10.0.0.5", 22),
            "deploy@10.0.0.5:22"
        );
        assert_eq!(
            endpoint_account("deploy", "10.0.0.5", 2222),
            "deploy@10.0.0.5:2222"
        );

        // sha512 hex is 128 chars, lowercase, and deterministic.
        let a = key_account_from_contents(b"-----BEGIN OPENSSH PRIVATE KEY-----\n");
        let b = key_account_from_contents(b"-----BEGIN OPENSSH PRIVATE KEY-----\n");
        assert_eq!(a, b);
        assert_eq!(a.len(), 128);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(a, key_account_from_contents(b"different"));
    }

    #[test]
    fn credential_kind_selects_service() {
        assert_eq!(CredentialKind::Password.service(), "tty7-ssh");
        assert_eq!(CredentialKind::KeyPassphrase.service(), "tty7-ssh-key");
    }

    #[test]
    fn credential_ref_round_trips_and_hides_secret() {
        let cref = CredentialRef::password("deploy", "10.0.0.5", 22);
        let json = serde_json::to_string(&cref).unwrap();
        // Only kind + account are serialized — never a secret.
        assert!(json.contains("deploy@10.0.0.5:22"));
        assert!(json.contains("password"));
        let back: CredentialRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cref);

        // A bad `kind` value falls back to the default rather than failing the parse.
        let lenient: CredentialRef =
            serde_json::from_str(r#"{"kind":"bogus","account":"x"}"#).unwrap();
        assert_eq!(lenient.kind, CredentialKind::Password);
        assert_eq!(lenient.account, "x");
    }
}
