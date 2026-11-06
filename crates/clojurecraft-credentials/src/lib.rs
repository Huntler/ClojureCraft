//! Access tokens kept in the operating system's credential store: the login Keychain on macOS and
//! the Secret Service (GNOME Keyring, KWallet) on Linux. Tokens are never written to
//! application files and are redacted from `Debug` output.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Service name under which ClojureCraft's credentials are stored.
pub const SERVICE: &str = "org.clojurecraft.ClojureCraft";
/// Account name of the Hugging Face access token.
pub const HUGGING_FACE_TOKEN: &str = "huggingface-token";

/// A credential value. Its `Debug` output never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CredentialError {
    /// No credential store can be reached (for example no Secret Service is running).
    #[error("{0}")]
    Unavailable(String),
    /// The store refused access (locked keyring, denied prompt).
    #[error("the credential store refused access: {0}")]
    Denied(String),
    #[error("the credential store reported an error: {0}")]
    Failed(String),
}

pub trait CredentialStore: Send + Sync {
    /// Human-readable store name, for example "macOS Keychain".
    fn name(&self) -> &str;
    fn get(&self, account: &str) -> Result<Option<Secret>, CredentialError>;
    fn set(&self, account: &str, secret: &Secret) -> Result<(), CredentialError>;
    /// Deleting a credential that does not exist succeeds.
    fn delete(&self, account: &str) -> Result<(), CredentialError>;
}

/// The credential store of this platform.
pub fn platform_store() -> Result<Arc<dyn CredentialStore>, CredentialError> {
    #[cfg(target_os = "linux")]
    {
        let store = zbus_secret_service_keyring_store::Store::new().map_err(|e| {
            CredentialError::Unavailable(format!(
                "No Secret Service is available ({e}). Install and start GNOME Keyring or KWallet to save access tokens; until then, tokens can only be used for the current session."
            ))
        })?;
        Ok(Arc::new(KeyringStore {
            name: "Secret Service".into(),
            store,
        }))
    }
    #[cfg(target_os = "macos")]
    {
        let store = apple_native_keyring_store::keychain::Store::new().map_err(|e| {
            CredentialError::Unavailable(format!("The macOS Keychain is not available ({e})."))
        })?;
        Ok(Arc::new(KeyringStore {
            name: "macOS Keychain".into(),
            store,
        }))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(CredentialError::Unavailable(
            "Saving access tokens is not supported on this platform.".into(),
        ))
    }
}

/// Adapter over a `keyring-core` store.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
struct KeyringStore {
    name: String,
    store: Arc<keyring_core::CredentialStore>,
}

fn map_err(e: keyring_core::Error) -> CredentialError {
    use keyring_core::Error as E;
    match e {
        E::NoStorageAccess(inner) => CredentialError::Denied(inner.to_string()),
        E::PlatformFailure(inner) => CredentialError::Unavailable(inner.to_string()),
        // Other variants may carry credential bytes; never include them.
        other => CredentialError::Failed(format!("{:?}", std::mem::discriminant(&other))),
    }
}

impl KeyringStore {
    fn entry(&self, account: &str) -> Result<keyring_core::Entry, CredentialError> {
        self.store.build(SERVICE, account, None).map_err(map_err)
    }
}

impl CredentialStore for KeyringStore {
    fn name(&self) -> &str {
        &self.name
    }

    fn get(&self, account: &str) -> Result<Option<Secret>, CredentialError> {
        match self.entry(account)?.get_password() {
            Ok(p) => Ok(Some(Secret(p))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_err(e)),
        }
    }

    fn set(&self, account: &str, secret: &Secret) -> Result<(), CredentialError> {
        self.entry(account)?
            .set_password(secret.expose())
            .map_err(map_err)
    }

    fn delete(&self, account: &str) -> Result<(), CredentialError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_err(e)),
        }
    }
}

/// A process-local store, for tests and for using a token without saving it.
#[derive(Default)]
pub struct MemoryStore {
    values: Mutex<HashMap<String, Secret>>,
}

impl CredentialStore for MemoryStore {
    fn name(&self) -> &str {
        "this session only"
    }

    fn get(&self, account: &str) -> Result<Option<Secret>, CredentialError> {
        Ok(self
            .values
            .lock()
            .map_err(|_| CredentialError::Failed("poisoned".into()))?
            .get(account)
            .cloned())
    }

    fn set(&self, account: &str, secret: &Secret) -> Result<(), CredentialError> {
        self.values
            .lock()
            .map_err(|_| CredentialError::Failed("poisoned".into()))?
            .insert(account.to_owned(), secret.clone());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), CredentialError> {
        self.values
            .lock()
            .map_err(|_| CredentialError::Failed("poisoned".into()))?
            .remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print() {
        let s = Secret::new("hf_abcdefghijklmnopqrstuvwxyz0123");
        assert_eq!(format!("{s:?}"), "Secret([redacted])");
        assert_eq!(format!("{:?}", Some(s.clone())), "Some(Secret([redacted]))");
    }

    #[test]
    fn memory_store_contract() {
        let store = MemoryStore::default();
        assert_eq!(store.get(HUGGING_FACE_TOKEN), Ok(None));
        store.set(HUGGING_FACE_TOKEN, &Secret::new("a")).expect("set");
        store.set(HUGGING_FACE_TOKEN, &Secret::new("b")).expect("replace");
        assert_eq!(store.get(HUGGING_FACE_TOKEN), Ok(Some(Secret::new("b"))));
        store.delete(HUGGING_FACE_TOKEN).expect("delete");
        store.delete(HUGGING_FACE_TOKEN).expect("delete twice");
        assert_eq!(store.get(HUGGING_FACE_TOKEN), Ok(None));
    }

    /// Talks to the real platform store; run explicitly with `--ignored` on a desktop session.
    #[test]
    #[ignore = "uses the real Keychain or Secret Service"]
    fn platform_store_round_trip() {
        let store = platform_store().expect("platform store");
        let account = "clojurecraft-test-entry";
        store
            .set(account, &Secret::new("test-value-0123456789"))
            .expect("set");
        assert_eq!(
            store.get(account).expect("get").map(|s| s.expose().to_owned()),
            Some("test-value-0123456789".into())
        );
        store.delete(account).expect("delete");
        assert_eq!(store.get(account).expect("get"), None);
    }
}
