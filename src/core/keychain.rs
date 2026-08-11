pub use tty7_core::core::keychain::{
    CredentialKind, CredentialRef, SERVICE_KEY_PASSPHRASE, SERVICE_PASSWORD, endpoint_account,
    key_account_from_contents,
};

#[derive(Debug)]
pub enum CredentialError {
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

pub type CredentialResult<T> = Result<T, CredentialError>;

pub trait CredentialStore: Send + Sync {
    fn get(&self, service: &str, account: &str) -> CredentialResult<Option<String>>;

    fn set(&self, service: &str, account: &str, secret: &str) -> CredentialResult<()>;

    fn delete(&self, service: &str, account: &str) -> CredentialResult<()>;

    fn password_for(&self, user: &str, host: &str, port: u16) -> CredentialResult<Option<String>> {
        self.get(SERVICE_PASSWORD, &endpoint_account(user, host, port))
    }

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

    fn delete_password(&self, user: &str, host: &str, port: u16) -> CredentialResult<()> {
        self.delete(SERVICE_PASSWORD, &endpoint_account(user, host, port))
    }

    fn passphrase_for_key(&self, key_sha512_hex: &str) -> CredentialResult<Option<String>> {
        self.get(SERVICE_KEY_PASSPHRASE, key_sha512_hex)
    }

    fn set_key_passphrase(
        &self,
        key_sha512_hex: &str,
        secret: &str,
    ) -> CredentialResult<CredentialRef> {
        self.set(SERVICE_KEY_PASSPHRASE, key_sha512_hex, secret)?;
        Ok(CredentialRef::key_passphrase(key_sha512_hex.to_string()))
    }

    fn delete_key_passphrase(&self, key_sha512_hex: &str) -> CredentialResult<()> {
        self.delete(SERVICE_KEY_PASSPHRASE, key_sha512_hex)
    }

    #[allow(dead_code)]
    fn get_ref(&self, cref: &CredentialRef) -> CredentialResult<Option<String>> {
        self.get(cref.service(), &cref.account)
    }

    #[allow(dead_code)]
    fn delete_ref(&self, cref: &CredentialRef) -> CredentialResult<()> {
        self.delete(cref.service(), &cref.account)
    }
}

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

#[cfg(test)]
#[derive(Debug, Default)]
pub struct InMemoryCredentialStore {
    entries: std::sync::Mutex<std::collections::HashMap<(String, String), String>>,
}

#[cfg(test)]
impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("credential store poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
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
    fn in_memory_store_get_set_delete() {
        let store = InMemoryCredentialStore::new();
        assert!(store.is_empty());

        assert_eq!(store.password_for("deploy", "host", 22).unwrap(), None);

        let cref = store.set_password("deploy", "host", 22, "hunter2").unwrap();
        assert_eq!(cref, CredentialRef::password("deploy", "host", 22));
        assert_eq!(store.get_ref(&cref).unwrap().as_deref(), Some("hunter2"));
        assert_eq!(
            store.password_for("deploy", "host", 22).unwrap().as_deref(),
            Some("hunter2")
        );

        store.set_password("deploy", "host", 22, "newpass").unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.password_for("deploy", "host", 22).unwrap().as_deref(),
            Some("newpass")
        );

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

        store.set_password("deploy", "host", 22, "pw").unwrap();
        assert_eq!(store.len(), 2);

        store.delete_key_passphrase(&key_id).unwrap();
        assert_eq!(store.passphrase_for_key(&key_id).unwrap(), None);
    }
}
