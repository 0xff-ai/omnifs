use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

const MOUNT_NAME_HINT: &str =
    "lowercase letters, digits, dashes; 1-32 chars; start with a letter/digit";

/// A validated mount identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MountName(String);

impl MountName {
    pub fn new(name: impl Into<String>) -> Result<Self, MountNameError> {
        let name = name.into();
        if name.is_empty() || name.len() > 32 {
            return Err(MountNameError::InvalidLength);
        }
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return Err(MountNameError::InvalidLength);
        };
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(MountNameError::InvalidStart);
        }
        for ch in chars {
            if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
                return Err(MountNameError::InvalidCharacter { ch });
            }
        }
        Ok(Self(name))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MountName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for MountName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Serialize for MountName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MountName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl FromStr for MountName {
    type Err = MountNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for MountName {
    type Error = MountNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for MountName {
    type Error = MountNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MountNameError {
    #[error("mount name must be 1-32 chars ({MOUNT_NAME_HINT})")]
    InvalidLength,
    #[error("mount name must start with a letter or digit ({MOUNT_NAME_HINT})")]
    InvalidStart,
    #[error("mount name contains invalid character `{ch}` ({MOUNT_NAME_HINT})")]
    InvalidCharacter { ch: char },
}
