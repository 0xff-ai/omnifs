//! In-memory credential store. Intended for use in tests only.

use crate::authn::CredentialId;
use crate::creds::{CredStoreError, CredentialEntry, CredentialStore};
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

/// Ephemeral credential store backed by a `Mutex<BTreeMap>`. Not persisted
/// across process restarts. Use in tests or as a mock.
#[derive(Default)]
pub struct MemoryStore {
    entries: Mutex<BTreeMap<String, CredentialEntry>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    fn entries(&self) -> Result<MutexGuard<'_, BTreeMap<String, CredentialEntry>>, CredStoreError> {
        self.entries
            .lock()
            .map_err(|_| CredStoreError::Backend("in-memory credential store lock poisoned".into()))
    }
}

impl CredentialStore for MemoryStore {
    fn put(&self, key: &CredentialId, entry: &CredentialEntry) -> Result<(), CredStoreError> {
        self.entries()?.insert(key.storage_key(), entry.clone());
        Ok(())
    }

    fn get(&self, key: &CredentialId) -> Result<Option<CredentialEntry>, CredStoreError> {
        Ok(self.entries()?.get(&key.storage_key()).cloned())
    }

    fn delete(&self, key: &CredentialId) -> Result<(), CredStoreError> {
        self.entries()?.remove(&key.storage_key());
        Ok(())
    }

    fn list(&self) -> Result<Option<Vec<CredentialId>>, CredStoreError> {
        let keys = self
            .entries()?
            .keys()
            .map(|storage_key| storage_key.parse())
            .collect::<Result<Vec<_>, _>>()
            .map_err(CredStoreError::from)?;
        Ok(Some(keys))
    }

    fn backend_label(&self) -> String {
        "in-memory (test only)".into()
    }
}
