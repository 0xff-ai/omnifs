//! Exact desired filesystem specifications.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use derive_more::{AsRef, Display};

pub const FILESYSTEM_GUEST_LOCATION: &str = "/omnifs";
const RUNTIME_INSTANCE_HINT: &str = "exactly 32 lowercase hexadecimal characters";

/// OS filesystem protocol exposed by a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemProtocol {
    Fuse,
    Nfs,
}

impl FilesystemProtocol {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fuse => "fuse",
            Self::Nfs => "nfs",
        }
    }
}

impl fmt::Display for FilesystemProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for FilesystemProtocol {
    type Err = ParseFilesystemProtocolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "fuse" => Ok(Self::Fuse),
            "nfs" => Ok(Self::Nfs),
            _ => Err(ParseFilesystemProtocolError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown Filesystem protocol `{0}`; expected fuse or nfs")]
pub struct ParseFilesystemProtocolError(String);

/// Runtime that owns one Filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilesystemRuntime {
    Host,
    Docker,
    Libkrun,
}

impl FilesystemRuntime {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::Libkrun => "libkrun",
        }
    }
}

impl fmt::Display for FilesystemRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for FilesystemRuntime {
    type Err = ParseFilesystemRuntimeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "host" => Ok(Self::Host),
            "docker" => Ok(Self::Docker),
            "libkrun" => Ok(Self::Libkrun),
            _ => Err(ParseFilesystemRuntimeError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown Filesystem runtime `{0}`; expected host, docker, or libkrun")]
pub struct ParseFilesystemRuntimeError(String);

/// Exact random identity of one launched Filesystem runtime.
///
/// Parsing this at process and wire ingress prevents malformed peers from
/// entering the live-session registry under an identity `SQLite` would reject.
#[derive(AsRef, Debug, Display, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[as_ref(str)]
pub struct RuntimeInstanceId(String);

impl RuntimeInstanceId {
    pub fn new(value: impl Into<String>) -> Result<Self, RuntimeInstanceIdError> {
        let value = value.into();
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(RuntimeInstanceIdError);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl FromStr for RuntimeInstanceId {
    type Err = RuntimeInstanceIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for RuntimeInstanceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RuntimeInstanceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("runtime instance must contain {RUNTIME_INSTANCE_HINT}")]
pub struct RuntimeInstanceIdError;

/// An exact filesystem configuration after daemon-owned normalization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemSpec {
    protocol: FilesystemProtocol,
    runtime: FilesystemRuntime,
    location: PathBuf,
    docker_image: Option<String>,
    libkrun_guest_image: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFilesystemSpec {
    protocol: FilesystemProtocol,
    runtime: FilesystemRuntime,
    location: PathBuf,
    docker_image: Option<String>,
    libkrun_guest_image: Option<String>,
}

impl<'de> Deserialize<'de> for FilesystemSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let stored = StoredFilesystemSpec::deserialize(deserializer)?;
        Self::new(
            stored.protocol,
            stored.runtime,
            stored.location,
            stored.docker_image,
            stored.libkrun_guest_image,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl FilesystemSpec {
    pub fn new(
        protocol: FilesystemProtocol,
        runtime: FilesystemRuntime,
        location: PathBuf,
        docker_image: Option<String>,
        libkrun_guest_image: Option<String>,
    ) -> Result<Self, FilesystemSpecError> {
        if !valid_pair(protocol, runtime) {
            return Err(FilesystemSpecError::UnsupportedPair { protocol, runtime });
        }
        match runtime {
            FilesystemRuntime::Host => {
                if !location.is_absolute() {
                    return Err(FilesystemSpecError::HostLocationNotAbsolute(location));
                }
                if docker_image.is_some() || libkrun_guest_image.is_some() {
                    return Err(FilesystemSpecError::HostAssets);
                }
            },
            FilesystemRuntime::Docker => {
                if location != Path::new(FILESYSTEM_GUEST_LOCATION) {
                    return Err(FilesystemSpecError::GuestLocation {
                        runtime,
                        actual: location,
                    });
                }
                if libkrun_guest_image.is_some() {
                    return Err(FilesystemSpecError::LibkrunAssetOnOtherRuntime { runtime });
                }
                validate_asset("docker image", docker_image.as_deref())?;
            },
            FilesystemRuntime::Libkrun => {
                if location != Path::new(FILESYSTEM_GUEST_LOCATION) {
                    return Err(FilesystemSpecError::GuestLocation {
                        runtime,
                        actual: location,
                    });
                }
                if docker_image.is_some() {
                    return Err(FilesystemSpecError::DockerAssetOnOtherRuntime { runtime });
                }
                validate_asset("libkrun guest image", libkrun_guest_image.as_deref())?;
            },
        }
        Ok(Self {
            protocol,
            runtime,
            location,
            docker_image,
            libkrun_guest_image,
        })
    }

    #[must_use]
    pub const fn protocol(&self) -> FilesystemProtocol {
        self.protocol
    }

    #[must_use]
    pub const fn runtime(&self) -> FilesystemRuntime {
        self.runtime
    }

    #[must_use]
    pub fn location(&self) -> &Path {
        &self.location
    }

    #[must_use]
    pub fn docker_image(&self) -> Option<&str> {
        self.docker_image.as_deref()
    }

    #[must_use]
    pub fn libkrun_guest_image(&self) -> Option<&str> {
        self.libkrun_guest_image.as_deref()
    }
}

fn validate_asset(field: &'static str, value: Option<&str>) -> Result<(), FilesystemSpecError> {
    if value.is_some_and(str::is_empty) {
        return Err(FilesystemSpecError::EmptyAsset { field });
    }
    Ok(())
}

fn valid_pair(protocol: FilesystemProtocol, runtime: FilesystemRuntime) -> bool {
    matches!(
        (protocol, runtime),
        (FilesystemProtocol::Nfs, FilesystemRuntime::Host)
            | (
                FilesystemProtocol::Fuse,
                FilesystemRuntime::Host | FilesystemRuntime::Docker | FilesystemRuntime::Libkrun
            )
    )
}

/// Whether the current daemon host can launch this protocol/runtime pair.
///
/// This is separate from [`FilesystemSpec`] parsing because Docker and
/// libkrun launch the Linux guest with the daemon's exact spec. A libkrun
/// guest must accept `fuse/libkrun` even though Linux cannot host libkrun.
#[must_use]
pub const fn filesystem_pair_supported_on_current_host(
    protocol: FilesystemProtocol,
    runtime: FilesystemRuntime,
) -> bool {
    match (protocol, runtime) {
        (FilesystemProtocol::Nfs, FilesystemRuntime::Host)
        | (FilesystemProtocol::Fuse, FilesystemRuntime::Docker) => {
            cfg!(any(target_os = "linux", target_os = "macos"))
        },
        (FilesystemProtocol::Fuse, FilesystemRuntime::Host) => cfg!(target_os = "linux"),
        (FilesystemProtocol::Fuse, FilesystemRuntime::Libkrun) => {
            cfg!(all(target_os = "macos", target_arch = "aarch64"))
        },
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilesystemSpecError {
    #[error("{protocol}/{runtime} is not a valid Filesystem protocol/runtime pair")]
    UnsupportedPair {
        protocol: FilesystemProtocol,
        runtime: FilesystemRuntime,
    },
    #[error("host filesystem location must be absolute: {}", .0.display())]
    HostLocationNotAbsolute(PathBuf),
    #[error("{runtime} owns its guest location; expected {FILESYSTEM_GUEST_LOCATION}, got {}", actual.display())]
    GuestLocation {
        runtime: FilesystemRuntime,
        actual: PathBuf,
    },
    #[error("host filesystems cannot have runtime image references")]
    HostAssets,
    #[error("docker image is only valid for the docker runtime, not {runtime}")]
    DockerAssetOnOtherRuntime { runtime: FilesystemRuntime },
    #[error("libkrun guest image is only valid for the libkrun runtime, not {runtime}")]
    LibkrunAssetOnOtherRuntime { runtime: FilesystemRuntime },
    #[error("{field} cannot be empty")]
    EmptyAsset { field: &'static str },
}

/// Content version of an exact filesystem specification.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FilesystemVersion([u8; 32]);

impl FilesystemVersion {
    #[must_use]
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for FilesystemVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for FilesystemVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "FilesystemVersion({self})")
    }
}

impl FromStr for FilesystemVersion {
    type Err = crate::ResourceDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let digest: crate::ResourceDigest = value.parse()?;
        Ok(Self(*digest.as_bytes()))
    }
}

impl Serialize for FilesystemVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for FilesystemVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_locations_assets_and_pairs() {
        assert!(
            FilesystemSpec::new(
                FilesystemProtocol::Fuse,
                FilesystemRuntime::Host,
                PathBuf::from("relative"),
                None,
                None
            )
            .is_err()
        );
        assert!(
            FilesystemSpec::new(
                FilesystemProtocol::Nfs,
                FilesystemRuntime::Docker,
                PathBuf::from(FILESYSTEM_GUEST_LOCATION),
                None,
                None
            )
            .is_err()
        );
        assert!(
            FilesystemSpec::new(
                FilesystemProtocol::Fuse,
                FilesystemRuntime::Host,
                PathBuf::from("/tmp/omnifs"),
                Some("image".into()),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn exact_libkrun_spec_round_trips_inside_its_linux_guest() {
        let spec = FilesystemSpec::new(
            FilesystemProtocol::Fuse,
            FilesystemRuntime::Libkrun,
            FILESYSTEM_GUEST_LOCATION.into(),
            None,
            Some("guest.raw".into()),
        )
        .unwrap();
        let encoded = serde_json::to_vec(&spec).unwrap();
        assert_eq!(
            serde_json::from_slice::<FilesystemSpec>(&encoded).unwrap(),
            spec
        );
    }

    #[test]
    fn host_support_is_distinct_from_exact_spec_validity() {
        assert_eq!(
            filesystem_pair_supported_on_current_host(
                FilesystemProtocol::Fuse,
                FilesystemRuntime::Libkrun
            ),
            cfg!(all(target_os = "macos", target_arch = "aarch64"))
        );
    }

    #[test]
    fn runtime_instance_identity_is_strict_at_parse_boundaries() {
        let valid = "0123456789abcdef0123456789abcdef";
        assert_eq!(RuntimeInstanceId::new(valid).unwrap().as_str(), valid);
        for invalid in [
            "",
            "0123456789abcdef",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789ABCDEF",
            "g123456789abcdef0123456789abcdef",
        ] {
            assert!(RuntimeInstanceId::new(invalid).is_err(), "{invalid}");
        }
    }
}
