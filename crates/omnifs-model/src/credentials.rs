use crate::ids::{AccountId, AuthSchemeId, ProviderId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Stable address for one host-managed HTTP credential.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialId {
    provider_id: ProviderId,
    scheme: AuthSchemeId,
    account: AccountId,
}

impl CredentialId {
    pub fn new(
        provider_id: impl Into<String>,
        scheme: impl Into<String>,
        account: impl Into<String>,
    ) -> Result<Self, CredentialIdError> {
        let provider_id = provider_id.into();
        let scheme = scheme.into();
        let account = account.into();
        Ok(Self::from_parts(
            ProviderId::new(provider_id.clone())
                .map_err(|_| CredentialIdError::invalid("provider_id", &provider_id))?,
            AuthSchemeId::new(scheme.clone())
                .map_err(|_| CredentialIdError::invalid("scheme", &scheme))?,
            AccountId::new(account.clone())
                .map_err(|_| CredentialIdError::invalid("account", &account))?,
        ))
    }

    pub fn from_parts(provider_id: ProviderId, scheme: AuthSchemeId, account: AccountId) -> Self {
        Self {
            provider_id,
            scheme,
            account,
        }
    }

    /// Stable storage account name used by keyring and file backends.
    pub fn storage_key(&self) -> String {
        format!("{}:{}:{}", self.provider_id, self.scheme, self.account)
    }

    pub fn provider_id(&self) -> &str {
        self.provider_id.as_str()
    }

    pub fn scheme(&self) -> &str {
        self.scheme.as_str()
    }

    pub fn account(&self) -> &str {
        self.account.as_str()
    }
}

impl fmt::Display for CredentialId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.storage_key())
    }
}

impl std::str::FromStr for CredentialId {
    type Err = CredentialIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.splitn(3, ':');
        let provider_id = parts
            .next()
            .ok_or_else(|| CredentialIdError::malformed_storage_key(s))?;
        let scheme = parts
            .next()
            .ok_or_else(|| CredentialIdError::malformed_storage_key(s))?;
        let account = parts
            .next()
            .ok_or_else(|| CredentialIdError::malformed_storage_key(s))?;
        Self::new(provider_id, scheme, account)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct CredentialIdWire {
    provider_id: String,
    scheme: String,
    account: String,
}

impl Serialize for CredentialId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        CredentialIdWire {
            provider_id: self.provider_id.to_string(),
            scheme: self.scheme.to_string(),
            account: self.account.to_string(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CredentialId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = CredentialIdWire::deserialize(deserializer)?;
        Self::new(wire.provider_id, wire.scheme, wire.account).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialIdError {
    #[error("invalid credential storage key: {value}")]
    MalformedStorageKey { value: String },
    #[error("credential {field} cannot be empty")]
    Empty { field: &'static str },
    #[error("credential {field} is too long: {len} bytes, max 128")]
    TooLong { field: &'static str, len: usize },
    #[error("invalid credential {field}: {value}")]
    InvalidPart { field: &'static str, value: String },
}

impl CredentialIdError {
    fn malformed_storage_key(value: &str) -> Self {
        Self::MalformedStorageKey {
            value: value.to_owned(),
        }
    }

    fn invalid(field: &'static str, value: &str) -> Self {
        if value.is_empty() {
            Self::Empty { field }
        } else if value.len() > 128 {
            Self::TooLong {
                field,
                len: value.len(),
            }
        } else {
            Self::InvalidPart {
                field,
                value: value.to_owned(),
            }
        }
    }
}
