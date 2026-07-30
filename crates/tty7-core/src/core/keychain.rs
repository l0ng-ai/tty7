use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

pub const SERVICE_PASSWORD: &str = "tty7-ssh";
pub const SERVICE_KEY_PASSPHRASE: &str = "tty7-ssh-key";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialKind {
    #[default]
    Password,
    KeyPassphrase,
}

impl CredentialKind {
    pub fn service(self) -> &'static str {
        match self {
            CredentialKind::Password => SERVICE_PASSWORD,
            CredentialKind::KeyPassphrase => SERVICE_KEY_PASSPHRASE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct CredentialRef {
    #[serde(deserialize_with = "crate::core::config::de_lenient")]
    pub kind: CredentialKind,
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
    pub fn password(user: &str, host: &str, port: u16) -> Self {
        Self {
            kind: CredentialKind::Password,
            account: endpoint_account(user, host, port),
        }
    }

    pub fn key_passphrase(key_sha512_hex: impl Into<String>) -> Self {
        Self {
            kind: CredentialKind::KeyPassphrase,
            account: key_sha512_hex.into(),
        }
    }

    pub fn service(&self) -> &'static str {
        self.kind.service()
    }
}

pub fn endpoint_account(user: &str, host: &str, port: u16) -> String {
    format!("{user}@{host}:{port}")
}

pub fn key_account_from_contents(key_bytes: &[u8]) -> String {
    let digest = Sha512::digest(key_bytes);
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
        assert!(json.contains("deploy@10.0.0.5:22"));
        assert!(json.contains("password"));
        let back: CredentialRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cref);

        let lenient: CredentialRef =
            serde_json::from_str(r#"{"kind":"bogus","account":"x"}"#).unwrap();
        assert_eq!(lenient.kind, CredentialKind::Password);
        assert_eq!(lenient.account, "x");
    }
}
