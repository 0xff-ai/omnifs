//! Keyring-backed credential store.
//!
//! Routes to the platform's native secret storage: macOS Keychain, Linux
//! Secret Service (via libsecret), or Windows Credential Manager. The
//! `keyring` crate handles all platform dispatch.
//!
//! Enumeration across stored accounts is not supported by the keyring
//! backend's API surface; `list()` returns `Ok(None)`.
//!
//! Exercised by the omnifs init integration test rather than by in-crate
//! unit tests, because the keyring requires a live platform secret store
//! that is absent in headless CI environments.

use crate::{CredentialEntry, CredentialStore, StoreError};
use keyring::{Entry, Error as KeyringError};
use omnifs_core::CredentialId;

/// Credential store backed by the OS native keychain.
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    /// Creates a store using `"omnifs"` as the keychain service name.
    pub fn new() -> Result<Self, StoreError> {
        Self::with_service("omnifs")
    }

    /// Creates a store using a custom service name. Useful in tests to avoid
    /// polluting the production keychain namespace.
    pub fn with_service(service: impl Into<String>) -> Result<Self, StoreError> {
        let service = service.into();
        // Probe for platform support without storing anything. Entry::new
        // alone is cheap; get_password forces an actual keychain roundtrip.
        let probe = Entry::new(&service, "__probe__")
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        // NoEntry is fine: the probe account simply hasn't been written yet.
        // Other non-availability errors (bad encoding, etc.) don't indicate
        // that the keychain is absent.
        if let Err(KeyringError::PlatformFailure(e) | KeyringError::NoStorageAccess(e)) =
            probe.get_password()
        {
            return Err(StoreError::Unavailable(e.to_string()));
        }
        Ok(Self { service })
    }
}

impl CredentialStore for KeyringStore {
    fn put(&self, key: &CredentialId, entry: &CredentialEntry) -> Result<(), StoreError> {
        let json = serde_json::to_string(entry)?;
        Entry::new(&self.service, &key.storage_key())
            .map_err(|e| StoreError::Backend(e.to_string()))?
            .set_password(&json)
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    fn get(&self, key: &CredentialId) -> Result<Option<CredentialEntry>, StoreError> {
        let entry = Entry::new(&self.service, &key.storage_key())
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        match entry.get_password() {
            Ok(json) => {
                let cred = serde_json::from_str(&json)?;
                Ok(Some(cred))
            },
            Err(KeyringError::NoEntry) => Ok(None),
            Err(e) => Err(StoreError::Backend(e.to_string())),
        }
    }

    fn delete(&self, key: &CredentialId) -> Result<(), StoreError> {
        let entry = Entry::new(&self.service, &key.storage_key())
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        match entry.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(e) => Err(StoreError::Backend(e.to_string())),
        }
    }

    fn list(&self) -> Result<Option<Vec<CredentialId>>, StoreError> {
        Ok(None)
    }

    fn backend_label(&self) -> String {
        format!("OS keychain (service: {})", self.service)
    }
}
