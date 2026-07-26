//! Named operating-system filesystem instances over the shared namespace.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const ID_HINT: &str = "lowercase letters, digits, dashes; 1-32 chars; start with a letter or digit";
pub const GUEST_LOCATION: &str = "/omnifs";

/// Stable workspace-wide identity for one configured filesystem.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(String);

impl Id {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        if value.is_empty() || value.len() > 32 {
            return Err(IdError::InvalidLength);
        }
        let mut chars = value.chars();
        let Some(first) = chars.next() else {
            return Err(IdError::InvalidLength);
        };
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(IdError::InvalidStart);
        }
        for ch in chars {
            if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
                return Err(IdError::InvalidCharacter { ch });
            }
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl FromStr for Id {
    type Err = IdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("filesystem name must be 1-32 chars ({ID_HINT})")]
    InvalidLength,
    #[error("filesystem name must start with a letter or digit ({ID_HINT})")]
    InvalidStart,
    #[error("filesystem name contains invalid character `{ch}` ({ID_HINT})")]
    InvalidCharacter { ch: char },
}

/// OS filesystem protocol exposed by an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Fuse,
    Nfs,
}

impl Protocol {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fuse => "fuse",
            Self::Nfs => "nfs",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Protocol {
    type Err = ParseProtocolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "fuse" => Ok(Self::Fuse),
            "nfs" => Ok(Self::Nfs),
            _ => Err(ParseProtocolError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown filesystem protocol `{0}`; expected fuse or nfs")]
pub struct ParseProtocolError(String);

/// Runtime that owns one filesystem instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Host,
    Docker,
    Libkrun,
}

impl Runtime {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::Libkrun => "libkrun",
        }
    }
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Runtime {
    type Err = ParseRuntimeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "host" => Ok(Self::Host),
            "docker" => Ok(Self::Docker),
            "libkrun" => Ok(Self::Libkrun),
            _ => Err(ParseRuntimeError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown filesystem runtime `{0}`; expected host, docker, or libkrun")]
pub struct ParseRuntimeError(String);

/// Fully resolved configuration and identity for one filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    id: Id,
    protocol: Protocol,
    runtime: Runtime,
    location: PathBuf,
}

impl Spec {
    pub fn new(
        id: Id,
        protocol: Protocol,
        runtime: Runtime,
        location: PathBuf,
    ) -> Result<Self, SpecError> {
        match runtime {
            Runtime::Host if !location.is_absolute() => {
                return Err(SpecError::HostLocationNotAbsolute(location));
            },
            Runtime::Docker | Runtime::Libkrun if location != Path::new(GUEST_LOCATION) => {
                return Err(SpecError::GuestLocation {
                    runtime,
                    actual: location,
                });
            },
            Runtime::Host | Runtime::Docker | Runtime::Libkrun => {},
        }
        Ok(Self {
            id,
            protocol,
            runtime,
            location,
        })
    }

    #[must_use]
    pub fn id(&self) -> &Id {
        &self.id
    }

    #[must_use]
    pub const fn protocol(&self) -> Protocol {
        self.protocol
    }

    #[must_use]
    pub const fn runtime(&self) -> Runtime {
        self.runtime
    }

    #[must_use]
    pub fn location(&self) -> &Path {
        &self.location
    }
}

impl fmt::Display for Spec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}/{}, {})",
            self.id,
            self.protocol,
            self.runtime,
            self.location.display()
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSpec {
    id: Id,
    protocol: Protocol,
    runtime: Runtime,
    location: PathBuf,
}

impl<'de> Deserialize<'de> for Spec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let stored = StoredSpec::deserialize(deserializer)?;
        Self::new(stored.id, stored.protocol, stored.runtime, stored.location)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("host filesystem location must be absolute: {}", .0.display())]
    HostLocationNotAbsolute(PathBuf),
    #[error(
        "{runtime} owns its filesystem location; expected {GUEST_LOCATION}, got {}",
        actual.display()
    )]
    GuestLocation { runtime: Runtime, actual: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_a_strict_workspace_key() {
        assert_eq!(Id::new("main-fs").unwrap().as_str(), "main-fs");
        assert!(Id::new("").is_err());
        assert!(Id::new("Main").is_err());
        assert!(Id::new("main/fs").is_err());
    }

    #[test]
    fn spec_rejects_invalid_runtime_locations_and_unknown_fields() {
        assert!(
            Spec::new(
                Id::new("host").unwrap(),
                Protocol::Nfs,
                Runtime::Host,
                PathBuf::from("relative"),
            )
            .is_err()
        );
        assert!(
            Spec::new(
                Id::new("guest").unwrap(),
                Protocol::Fuse,
                Runtime::Docker,
                PathBuf::from("/other"),
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<Spec>(serde_json::json!({
                "id": "main",
                "protocol": "fuse",
                "runtime": "docker",
                "location": "/omnifs",
                "unknown": true
            }))
            .is_err()
        );
    }
}
