use std::str::FromStr;

use derive_more::{AsRef, Display};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ProviderId;

const KEY_PART_HINT: &str = "letters, digits, dashes, underscores, or dots; 1-128 chars";

/// Provider name slug: the catalog index and UI label, never content identity.
/// This is the human-facing provider name (e.g. `github`), the slug
/// credentials are keyed by, distinct from the content [`ProviderId`] hash.
#[derive(AsRef, Debug, Display, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[as_ref(str)]
pub struct ProviderName(String);

impl ProviderName {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        validate_key_part("provider_name", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ProviderName {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ProviderName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    #[error("{field} cannot be empty ({KEY_PART_HINT})")]
    Empty { field: &'static str },
    #[error("{field} is too long: {len} bytes, max 128")]
    TooLong { field: &'static str, len: usize },
    #[error("invalid {field} `{value}` ({KEY_PART_HINT})")]
    Invalid { field: &'static str, value: String },
    #[error("account cannot be empty")]
    AccountEmpty,
    #[error("account is too long: {len} bytes, max 128")]
    AccountTooLong { len: usize },
    #[error("invalid account `{value}`")]
    InvalidAccount { value: String },
}

pub fn validate_key_part(field: &'static str, value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty { field });
    }
    if value.len() > 128 {
        return Err(IdError::TooLong {
            field,
            len: value.len(),
        });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(IdError::Invalid {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

pub fn validate_account(value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::AccountEmpty);
    }
    if value.len() > 128 {
        return Err(IdError::AccountTooLong { len: value.len() });
    }
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(IdError::InvalidAccount {
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// Provider-stated version label, taken from the manifest `version` field.
/// Informational catalog/UI context, never identity.
#[derive(Clone, Debug, Display, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderVersion(String);

impl ProviderVersion {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Catalog/UI context carried alongside a pinned provider; never identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderMeta {
    pub name: ProviderName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ProviderVersion>,
}

/// A mount's pinned provider reference: the content [`ProviderId`] plus the
/// [`ProviderMeta`] context resolved at pin time. This is what a mount spec
/// stores and what the daemon resolves to serve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRef {
    pub id: ProviderId,
    pub meta: ProviderMeta,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_accepts_slug_and_rejects_invalid() {
        assert_eq!(ProviderName::new("github").unwrap().as_str(), "github");
        assert!(ProviderName::new("bad id!").is_err());
        assert!(ProviderName::new("").is_err());
        assert_eq!(
            serde_json::to_string(&ProviderName::new("github").unwrap()).unwrap(),
            "\"github\""
        );
        assert!(serde_json::from_str::<ProviderName>("\"bad id!\"").is_err());
    }
}
