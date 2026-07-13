//! The SSH credential vault: an abstraction over the OS keychain.
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
//! The [`CredentialStore`] trait keeps the backend swappable: [`OsCredentialStore`]
//! talks to the real keychain (macOS Keychain / Windows Credential Manager / Linux
//! Secret Service via the `keyring` crate), while [`InMemoryCredentialStore`] backs
//! tests without touching the machine's keychain.
//!
//! Secrets are never logged. The typed helpers below deliberately keep secret
//! values out of `Debug`/log output.

use std::collections::HashMap;
use std::sync::Mutex;

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

/// A backend failure while talking to the credential store. Intentionally never
/// carries a secret value — only a human-readable reason from the backend.
#[derive(Debug)]
pub enum CredentialError {
    /// The underlying store failed (keychain locked, access denied, IO error).
    Backend(String),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::Backend(reason) => write!(f, "credential store error: {reason}"),
        }
    }
}

impl std::error::Error for CredentialError {}

/// Result alias for credential-store operations.
pub type CredentialResult<T> = Result<T, CredentialError>;

/// A secret store keyed by `(service, account)`. Implementors talk to a real OS
/// keychain or an in-memory map.
///
/// Contract:
/// - `get` returns `Ok(None)` when the entry is absent (not an error).
/// - `delete` is idempotent: deleting an absent entry returns `Ok(())`.
/// - Implementors must never log secret values.
pub trait CredentialStore: Send + Sync {
    /// Fetch the secret for `(service, account)`, or `Ok(None)` if absent.
    fn get(&self, service: &str, account: &str) -> CredentialResult<Option<String>>;

    /// Store `secret` under `(service, account)`, overwriting any existing value.
    fn set(&self, service: &str, account: &str, secret: &str) -> CredentialResult<()>;

    /// Remove the entry at `(service, account)`. Absent entry ⇒ `Ok(())`.
    fn delete(&self, service: &str, account: &str) -> CredentialResult<()>;

    // ── Typed endpoint/key helpers (default methods over get/set/delete) ──────

    /// The stored password for an endpoint, if any.
    fn password_for(&self, user: &str, host: &str, port: u16) -> CredentialResult<Option<String>> {
        self.get(SERVICE_PASSWORD, &endpoint_account(user, host, port))
    }

    /// Store a password for an endpoint and return the [`CredentialRef`] naming it.
    fn set_password(
        &self,
        user: &str,
        host: &str,
        port: u16,
        secret: &str,
    ) -> CredentialResult<CredentialRef> {
        let account = endpoint_account(user, host, port);
        self.set(SERVICE_PASSWORD, &account, secret)?;
        Ok(CredentialRef {
            kind: CredentialKind::Password,
            account,
        })
    }

    /// Delete the stored password for an endpoint (idempotent).
    fn delete_password(&self, user: &str, host: &str, port: u16) -> CredentialResult<()> {
        self.delete(SERVICE_PASSWORD, &endpoint_account(user, host, port))
    }

    /// The stored passphrase for a private key (keyed by its sha512-hex), if any.
    fn passphrase_for_key(&self, key_sha512_hex: &str) -> CredentialResult<Option<String>> {
        self.get(SERVICE_KEY_PASSPHRASE, key_sha512_hex)
    }

    /// Store a passphrase for a private key and return the [`CredentialRef`].
    fn set_key_passphrase(
        &self,
        key_sha512_hex: &str,
        secret: &str,
    ) -> CredentialResult<CredentialRef> {
        self.set(SERVICE_KEY_PASSPHRASE, key_sha512_hex, secret)?;
        Ok(CredentialRef::key_passphrase(key_sha512_hex.to_string()))
    }

    /// Delete the stored passphrase for a private key (idempotent).
    fn delete_key_passphrase(&self, key_sha512_hex: &str) -> CredentialResult<()> {
        self.delete(SERVICE_KEY_PASSPHRASE, key_sha512_hex)
    }

    /// Resolve a [`CredentialRef`] to its secret, or `Ok(None)` if absent.
    fn get_ref(&self, cref: &CredentialRef) -> CredentialResult<Option<String>> {
        self.get(cref.service(), &cref.account)
    }

    /// Delete the entry a [`CredentialRef`] names (idempotent).
    fn delete_ref(&self, cref: &CredentialRef) -> CredentialResult<()> {
        self.delete(cref.service(), &cref.account)
    }
}

/// The production store backed by the OS keychain via the `keyring` crate.
///
/// `keyring` 4.x's default `v1` feature auto-selects the platform store on first
/// use, so this needs no per-platform wiring. A missing entry surfaces as
/// `Ok(None)`; every other failure becomes [`CredentialError::Backend`] with the
/// backend's message (never a secret).
#[derive(Debug, Default, Clone, Copy)]
pub struct OsCredentialStore;

impl CredentialStore for OsCredentialStore {
    fn get(&self, service: &str, account: &str) -> CredentialResult<Option<String>> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| CredentialError::Backend(e.to_string()))?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CredentialError::Backend(e.to_string())),
        }
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> CredentialResult<()> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| CredentialError::Backend(e.to_string()))?;
        entry
            .set_password(secret)
            .map_err(|e| CredentialError::Backend(e.to_string()))
    }

    fn delete(&self, service: &str, account: &str) -> CredentialResult<()> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| CredentialError::Backend(e.to_string()))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(CredentialError::Backend(e.to_string())),
        }
    }
}

/// An in-memory store for tests. Never touches the OS keychain.
#[derive(Debug, Default)]
pub struct InMemoryCredentialStore {
    // Keyed by (service, account). Behind a Mutex so the store is `Sync` and can
    // be shared like the real one.
    entries: Mutex<HashMap<(String, String), String>>,
}

impl InMemoryCredentialStore {
    /// A fresh, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored entries (test introspection).
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("credential store poisoned")
            .len()
    }

    /// Whether the store holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn get(&self, service: &str, account: &str) -> CredentialResult<Option<String>> {
        let entries = self.entries.lock().expect("credential store poisoned");
        Ok(entries
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> CredentialResult<()> {
        let mut entries = self.entries.lock().expect("credential store poisoned");
        entries.insert(
            (service.to_string(), account.to_string()),
            secret.to_string(),
        );
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> CredentialResult<()> {
        let mut entries = self.entries.lock().expect("credential store poisoned");
        entries.remove(&(service.to_string(), account.to_string()));
        Ok(())
    }
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

    #[test]
    fn in_memory_store_get_set_delete() {
        let store = InMemoryCredentialStore::new();
        assert!(store.is_empty());

        // Absent → None (not an error).
        assert_eq!(store.password_for("deploy", "host", 22).unwrap(), None);

        // Set returns a ref that resolves back to the secret.
        let cref = store.set_password("deploy", "host", 22, "hunter2").unwrap();
        assert_eq!(cref, CredentialRef::password("deploy", "host", 22));
        assert_eq!(store.get_ref(&cref).unwrap().as_deref(), Some("hunter2"));
        assert_eq!(
            store.password_for("deploy", "host", 22).unwrap().as_deref(),
            Some("hunter2")
        );

        // Overwrite replaces in place (endpoint keying — one entry per endpoint).
        store.set_password("deploy", "host", 22, "newpass").unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.password_for("deploy", "host", 22).unwrap().as_deref(),
            Some("newpass")
        );

        // Delete is idempotent.
        store.delete_password("deploy", "host", 22).unwrap();
        assert_eq!(store.password_for("deploy", "host", 22).unwrap(), None);
        store.delete_password("deploy", "host", 22).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn key_passphrase_helpers_use_the_key_service() {
        let store = InMemoryCredentialStore::new();
        let key_id = key_account_from_contents(b"encrypted-key-bytes");
        assert_eq!(store.passphrase_for_key(&key_id).unwrap(), None);

        let cref = store.set_key_passphrase(&key_id, "s3cret").unwrap();
        assert_eq!(cref.kind, CredentialKind::KeyPassphrase);
        assert_eq!(cref.service(), "tty7-ssh-key");
        assert_eq!(
            store.passphrase_for_key(&key_id).unwrap().as_deref(),
            Some("s3cret")
        );

        // A password with the same account string does NOT collide (different service).
        store.set_password("deploy", "host", 22, "pw").unwrap();
        assert_eq!(store.len(), 2);

        store.delete_key_passphrase(&key_id).unwrap();
        assert_eq!(store.passphrase_for_key(&key_id).unwrap(), None);
    }
}
