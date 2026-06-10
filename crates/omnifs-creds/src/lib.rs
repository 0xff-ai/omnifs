pub mod file_store;
pub mod keyring_store;
pub mod memory_store;
pub use file_store::FileStore;
pub use keyring_store::KeyringStore;
pub use memory_store::MemoryStore;

use secrecy::SecretString;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use time::OffsetDateTime;

use omnifs_core::{CredentialId, CredentialIdError};

/// Re-export so callers don't need a direct omnifs-core dep for this type.
pub use omnifs_core::AuthKind;

/// One durable host-managed HTTP credential entry.
#[derive(Debug, Clone)]
pub struct CredentialEntry {
    kind: AuthKind,
    value: SecretString,
    stored_at: OffsetDateTime,
    last_validated: Option<OffsetDateTime>,
    scopes: Vec<String>,
    /// Human-readable identity reported by the upstream API at validation time.
    upstream_identity: Option<String>,
    refresh_token: Option<SecretString>,
    expires_at: Option<OffsetDateTime>,
    token_type: String,
    extras: BTreeMap<String, String>,
}

impl CredentialEntry {
    pub fn static_token(access_token: SecretString, stored_at: OffsetDateTime) -> Self {
        Self {
            kind: AuthKind::StaticToken,
            value: access_token,
            stored_at,
            last_validated: None,
            scopes: vec![],
            upstream_identity: None,
            refresh_token: None,
            expires_at: None,
            token_type: "Bearer".to_owned(),
            extras: BTreeMap::new(),
        }
    }

    pub fn oauth(
        access_token: SecretString,
        refresh_token: Option<SecretString>,
        expires_at: Option<OffsetDateTime>,
        token_type: impl Into<String>,
        scopes: Vec<String>,
        stored_at: OffsetDateTime,
    ) -> Self {
        let token_type = token_type.into();
        Self {
            kind: AuthKind::OAuth,
            value: access_token,
            stored_at,
            last_validated: None,
            scopes,
            upstream_identity: None,
            refresh_token,
            expires_at,
            token_type: if token_type.is_empty() {
                "Bearer".to_owned()
            } else {
                token_type
            },
            extras: BTreeMap::new(),
        }
    }

    pub fn kind(&self) -> AuthKind {
        self.kind
    }

    pub fn access_token(&self) -> &SecretString {
        &self.value
    }

    pub fn refresh_token(&self) -> Option<SecretString> {
        self.refresh_token.clone()
    }

    pub fn expires_at(&self) -> Option<OffsetDateTime> {
        self.expires_at
    }

    pub fn token_type(&self) -> &str {
        &self.token_type
    }

    pub fn stored_at(&self) -> OffsetDateTime {
        self.stored_at
    }

    pub fn last_validated(&self) -> Option<OffsetDateTime> {
        self.last_validated
    }

    pub fn set_last_validated(&mut self, last_validated: Option<OffsetDateTime>) {
        self.last_validated = last_validated;
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    pub fn into_scopes(self) -> Vec<String> {
        self.scopes
    }

    pub fn upstream_identity(&self) -> Option<&str> {
        self.upstream_identity.as_deref()
    }

    pub fn set_upstream_identity(&mut self, upstream_identity: Option<String>) {
        self.upstream_identity = upstream_identity;
    }

    pub fn extras(&self) -> &BTreeMap<String, String> {
        &self.extras
    }

    pub fn set_extras(&mut self, extras: BTreeMap<String, String>) {
        self.extras = extras;
    }
}

#[derive(Deserialize)]
struct CredentialEntryWire {
    kind: AuthKind,
    #[serde(rename = "access_token", with = "secret_string_serde")]
    access_token: SecretString,
    #[serde(default, with = "secret_string_serde::option")]
    refresh_token: Option<SecretString>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
    #[serde(default)]
    token_type: String,
    #[serde(with = "time::serde::rfc3339")]
    stored_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    last_validated: Option<OffsetDateTime>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    upstream_identity: Option<String>,
    #[serde(default)]
    extras: BTreeMap<String, String>,
}

impl From<&CredentialEntry> for CredentialEntryWire {
    fn from(entry: &CredentialEntry) -> Self {
        Self {
            kind: entry.kind,
            access_token: entry.value.clone(),
            refresh_token: entry.refresh_token.clone(),
            expires_at: entry.expires_at,
            token_type: entry.token_type.clone(),
            stored_at: entry.stored_at,
            last_validated: entry.last_validated,
            scopes: entry.scopes.clone(),
            upstream_identity: entry.upstream_identity.clone(),
            extras: entry.extras.clone(),
        }
    }
}

impl Serialize for CredentialEntryWire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use secrecy::ExposeSecret;
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("CredentialEntry", 10)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("access_token", self.access_token.expose_secret())?;
        state.serialize_field(
            "refresh_token",
            &self.refresh_token.as_ref().map(ExposeSecret::expose_secret),
        )?;
        let expires_at = self.expires_at.map(|value| {
            value
                .format(&time::format_description::well_known::Rfc3339)
                .expect("RFC3339 formatting should succeed")
        });
        state.serialize_field("expires_at", &expires_at)?;
        state.serialize_field("token_type", &self.token_type)?;
        state.serialize_field(
            "stored_at",
            &self
                .stored_at
                .format(&time::format_description::well_known::Rfc3339)
                .expect("RFC3339 formatting should succeed"),
        )?;
        let last_validated = self.last_validated.map(|value| {
            value
                .format(&time::format_description::well_known::Rfc3339)
                .expect("RFC3339 formatting should succeed")
        });
        state.serialize_field("last_validated", &last_validated)?;
        state.serialize_field("scopes", &self.scopes)?;
        state.serialize_field("upstream_identity", &self.upstream_identity)?;
        state.serialize_field("extras", &self.extras)?;
        state.end()
    }
}

impl Serialize for CredentialEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        CredentialEntryWire::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CredentialEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = CredentialEntryWire::deserialize(deserializer)?;
        let entry = Self {
            kind: wire.kind,
            value: wire.access_token,
            stored_at: wire.stored_at,
            last_validated: wire.last_validated,
            scopes: wire.scopes,
            upstream_identity: wire.upstream_identity,
            refresh_token: wire.refresh_token,
            expires_at: wire.expires_at,
            token_type: if wire.token_type.is_empty() {
                "Bearer".to_owned()
            } else {
                wire.token_type
            },
            extras: wire.extras,
        };
        Ok(entry)
    }
}

/// Trait every credential store implements. Errors are typed so callers can
/// decide how to handle each variant.
pub trait CredentialStore: Send + Sync {
    fn put(&self, key: &CredentialId, entry: &CredentialEntry) -> Result<(), StoreError>;
    fn get(&self, key: &CredentialId) -> Result<Option<CredentialEntry>, StoreError>;
    fn delete(&self, key: &CredentialId) -> Result<(), StoreError>;
    /// Lists known credential ids. Backends that cannot enumerate return
    /// `Ok(None)`.
    fn list(&self) -> Result<Option<Vec<CredentialId>>, StoreError>;
    /// Human-readable backend name shown in UX (e.g. "macOS Keychain",
    /// "file: ~/.omnifs/data/credentials.json").
    fn backend_label(&self) -> String;
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("backend error: {0}")]
    Backend(String),
    #[error(transparent)]
    CredentialId(#[from] CredentialIdError),
}

mod secret_string_serde {
    use secrecy::SecretString;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<SecretString, D::Error> {
        Ok(SecretString::from(String::deserialize(de)?))
    }

    pub mod option {
        use secrecy::SecretString;
        use serde::{Deserialize, Deserializer};

        pub fn deserialize<'de, D: Deserializer<'de>>(
            de: D,
        ) -> Result<Option<SecretString>, D::Error> {
            Ok(Option::<String>::deserialize(de)?.map(SecretString::from))
        }
    }
}
