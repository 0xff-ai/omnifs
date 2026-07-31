//! Typed identities and versions for daemon-owned desired resources.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

use derive_more::{AsRef, Display};

const NAME_HINT: &str =
    "lowercase letters, digits, dashes; 1-32 chars; start with a letter or digit";

/// One name grammar shared by every desired resource kind.
#[derive(AsRef, Debug, Display, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[as_ref(str)]
pub struct ResourceName(String);

impl ResourceName {
    pub fn new(value: impl Into<String>) -> Result<Self, ResourceNameError> {
        let value = value.into();
        if value.len() > 32 {
            return Err(ResourceNameError::InvalidLength);
        }

        let mut chars = value.chars();
        let first = chars.next().ok_or(ResourceNameError::InvalidLength)?;
        if !matches!(first, 'a'..='z' | '0'..='9') {
            return Err(ResourceNameError::InvalidStart);
        }

        if let Some(ch) = chars.find(|&ch| !matches!(ch, 'a'..='z' | '0'..='9' | '-')) {
            return Err(ResourceNameError::InvalidCharacter { ch });
        }

        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ResourceName {
    type Err = ResourceNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for ResourceName {
    type Error = ResourceNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for ResourceName {
    type Error = ResourceNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for ResourceName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ResourceName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResourceNameError {
    #[error("resource name must be 1-32 chars ({NAME_HINT})")]
    InvalidLength,
    #[error("resource name must start with a letter or digit ({NAME_HINT})")]
    InvalidStart,
    #[error("resource name contains invalid character `{ch}` ({NAME_HINT})")]
    InvalidCharacter { ch: char },
}

/// The closed set of desired resource kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Provider,
    Credential,
    Mount,
    Filesystem,
}

impl ResourceKind {
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Provider => 1,
            Self::Credential => 2,
            Self::Mount => 3,
            Self::Filesystem => 4,
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Provider => "Provider",
            Self::Credential => "Credential",
            Self::Mount => "Mount",
            Self::Filesystem => "Filesystem",
        })
    }
}

/// Stable identity of one resource in a desired set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceKey {
    pub kind: ResourceKind,
    pub name: ResourceName,
}

impl ResourceKey {
    #[must_use]
    pub const fn new(kind: ResourceKind, name: ResourceName) -> Self {
        Self { kind, name }
    }
}

impl fmt::Display for ResourceKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.kind, self.name)
    }
}

/// Monotonic revision of the complete desired resource set.
#[derive(
    Debug,
    Display,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
)]
#[serde(transparent)]
pub struct ResourceRevision(u64);

impl ResourceRevision {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl FromStr for ResourceRevision {
    type Err = ResourceRevisionParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse()
            .map(Self)
            .map_err(|_| ResourceRevisionParseError)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("resource revision must be an unsigned integer")]
pub struct ResourceRevisionParseError;

/// Versioned digest of a normalized desired resource set.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceDigest([u8; 32]);

impl ResourceDigest {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ResourceDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ResourceDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ResourceDigest({self})")
    }
}

impl FromStr for ResourceDigest {
    type Err = ResourceDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(ResourceDigestParseError::BadLength { len: value.len() });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ResourceDigestParseError::NotLowerHex);
        }
        let mut bytes = [0_u8; 32];
        hex::decode_to_slice(value, &mut bytes)
            .map_err(|_| ResourceDigestParseError::NotLowerHex)?;
        Ok(Self(bytes))
    }
}

impl Serialize for ResourceDigest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ResourceDigest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResourceDigestParseError {
    #[error("resource digest must be 64 lowercase hex characters, got {len}")]
    BadLength { len: usize },
    #[error("resource digest must contain only lowercase hex characters")]
    NotLowerHex,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_name_uses_the_common_current_grammar() {
        for valid in ["github", "0", "dev-host", "a123"] {
            assert!(ResourceName::new(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "Upper", "-first", "slash/name", &"a".repeat(33)] {
            assert!(ResourceName::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn digest_round_trips_as_lower_hex() {
        let digest = ResourceDigest::from_bytes([9; 32]);
        assert_eq!(
            digest.to_string().parse::<ResourceDigest>().unwrap(),
            digest
        );
        assert!("A".repeat(64).parse::<ResourceDigest>().is_err());
    }

    #[test]
    fn revision_round_trips_through_display_and_parse() {
        assert_eq!("42".parse::<ResourceRevision>().unwrap().get(), 42);
        assert!("-1".parse::<ResourceRevision>().is_err());
    }
}
