use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const KEY_PART_HINT: &str = "letters, digits, dashes, underscores, or dots; 1-128 chars";

/// Provider name slug: the catalog index and UI label, never content identity.
/// This is the human-facing provider name (e.g. `github`), the slug
/// credentials are keyed by, distinct from the content [`ProviderId`] hash.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

impl fmt::Display for ProviderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ProviderName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for ProviderName {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
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

pub(crate) fn validate_key_part(field: &'static str, value: &str) -> Result<(), IdError> {
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

pub(crate) fn validate_account(value: &str) -> Result<(), IdError> {
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

/// Content identity of a provider: the BLAKE3 digest of the exact provider WASM
/// bytes the host holds. Mounts pin this so serving resolves by content, never
/// by name. Distinct from the [`ProviderName`] slug.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProviderId([u8; 32]);

impl ProviderId {
    #[must_use]
    pub fn from_wasm_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProviderId({self})")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderIdHexError {
    #[error("provider id must be 64 hex characters, got {len}")]
    BadLength { len: usize },
    #[error("provider id must be lowercase hex (0-9a-f)")]
    NotHex,
}

impl FromStr for ProviderId {
    type Err = ProviderIdHexError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(ProviderIdHexError::BadLength { len: value.len() });
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ProviderIdHexError::NotHex);
        }
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(value, &mut bytes).map_err(|_| ProviderIdHexError::NotHex)?;
        Ok(Self(bytes))
    }
}

impl Serialize for ProviderId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// Provider-stated version label, taken from the manifest `version` field.
/// Informational catalog/UI context, never identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderVersion(String);

impl ProviderVersion {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Catalog/UI context carried alongside a pinned provider; never identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMeta {
    pub name: ProviderName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ProviderVersion>,
}

/// A mount's pinned provider reference: the content [`ProviderId`] plus the
/// [`ProviderMeta`] context resolved at pin time. This is what a mount spec
/// stores and what the daemon resolves to serve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    }

    #[test]
    fn provider_id_validation() {
        let uppercase = "A".repeat(64);
        let bad_char = "g".repeat(64);
        for (label, hex) in [
            ("non-hex", "xyz"),
            ("uppercase", uppercase.as_str()),
            ("bad-char", bad_char.as_str()),
        ] {
            assert!(hex.parse::<ProviderId>().is_err(), "{label}");
        }

        let id = ProviderId::from_wasm_bytes(b"some wasm bytes");
        let display = id.to_string();
        assert_eq!(display.len(), 64, "wasm hash hex length");
        assert!(
            display
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "wasm hash must be lowercase hex"
        );
        assert_eq!(display.parse::<ProviderId>().unwrap(), id);
        assert_eq!(ProviderId::from_wasm_bytes(b"some wasm bytes"), id);
        assert_ne!(ProviderId::from_wasm_bytes(b"other bytes"), id);

        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<ProviderId>(&json).unwrap(), id);
    }
}
