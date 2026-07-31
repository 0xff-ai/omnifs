//! Exact desired filesystem attachment specifications.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub const ATTACHMENT_GUEST_LOCATION: &str = "/omnifs";
const RUNTIME_INSTANCE_HINT: &str = "exactly 32 lowercase hexadecimal characters";

/// OS filesystem protocol exposed by an Attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentProtocol {
    Fuse,
    Nfs,
}

impl AttachmentProtocol {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fuse => "fuse",
            Self::Nfs => "nfs",
        }
    }
}

impl fmt::Display for AttachmentProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AttachmentProtocol {
    type Err = ParseAttachmentProtocolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "fuse" => Ok(Self::Fuse),
            "nfs" => Ok(Self::Nfs),
            _ => Err(ParseAttachmentProtocolError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown Attachment protocol `{0}`; expected fuse or nfs")]
pub struct ParseAttachmentProtocolError(String);

/// Runtime that owns one Attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentRuntime {
    Host,
    Docker,
    Libkrun,
}

impl AttachmentRuntime {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::Libkrun => "libkrun",
        }
    }
}

impl fmt::Display for AttachmentRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AttachmentRuntime {
    type Err = ParseAttachmentRuntimeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "host" => Ok(Self::Host),
            "docker" => Ok(Self::Docker),
            "libkrun" => Ok(Self::Libkrun),
            _ => Err(ParseAttachmentRuntimeError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown Attachment runtime `{0}`; expected host, docker, or libkrun")]
pub struct ParseAttachmentRuntimeError(String);

/// Exact random identity of one launched Attachment runtime.
///
/// Parsing this at process and wire ingress prevents malformed peers from
/// entering the live-session registry under an identity `SQLite` would reject.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

impl fmt::Display for RuntimeInstanceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
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

/// An exact attachment configuration after daemon-owned normalization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentSpec {
    protocol: AttachmentProtocol,
    runtime: AttachmentRuntime,
    location: PathBuf,
    docker_image: Option<String>,
    libkrun_guest_image: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAttachmentSpec {
    protocol: AttachmentProtocol,
    runtime: AttachmentRuntime,
    location: PathBuf,
    docker_image: Option<String>,
    libkrun_guest_image: Option<String>,
}

impl<'de> Deserialize<'de> for AttachmentSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let stored = StoredAttachmentSpec::deserialize(deserializer)?;
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

impl AttachmentSpec {
    pub fn new(
        protocol: AttachmentProtocol,
        runtime: AttachmentRuntime,
        location: PathBuf,
        docker_image: Option<String>,
        libkrun_guest_image: Option<String>,
    ) -> Result<Self, AttachmentSpecError> {
        if !valid_pair(protocol, runtime) {
            return Err(AttachmentSpecError::UnsupportedPair { protocol, runtime });
        }
        match runtime {
            AttachmentRuntime::Host => {
                if !location.is_absolute() {
                    return Err(AttachmentSpecError::HostLocationNotAbsolute(location));
                }
                if docker_image.is_some() || libkrun_guest_image.is_some() {
                    return Err(AttachmentSpecError::HostAssets);
                }
            },
            AttachmentRuntime::Docker => {
                if location != Path::new(ATTACHMENT_GUEST_LOCATION) {
                    return Err(AttachmentSpecError::GuestLocation {
                        runtime,
                        actual: location,
                    });
                }
                if libkrun_guest_image.is_some() {
                    return Err(AttachmentSpecError::LibkrunAssetOnOtherRuntime { runtime });
                }
                validate_asset("docker image", docker_image.as_deref())?;
            },
            AttachmentRuntime::Libkrun => {
                if location != Path::new(ATTACHMENT_GUEST_LOCATION) {
                    return Err(AttachmentSpecError::GuestLocation {
                        runtime,
                        actual: location,
                    });
                }
                if docker_image.is_some() {
                    return Err(AttachmentSpecError::DockerAssetOnOtherRuntime { runtime });
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
    pub const fn protocol(&self) -> AttachmentProtocol {
        self.protocol
    }

    #[must_use]
    pub const fn runtime(&self) -> AttachmentRuntime {
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

fn validate_asset(field: &'static str, value: Option<&str>) -> Result<(), AttachmentSpecError> {
    if value.is_some_and(str::is_empty) {
        return Err(AttachmentSpecError::EmptyAsset { field });
    }
    Ok(())
}

fn valid_pair(protocol: AttachmentProtocol, runtime: AttachmentRuntime) -> bool {
    matches!(
        (protocol, runtime),
        (AttachmentProtocol::Nfs, AttachmentRuntime::Host)
            | (
                AttachmentProtocol::Fuse,
                AttachmentRuntime::Host | AttachmentRuntime::Docker | AttachmentRuntime::Libkrun
            )
    )
}

/// Whether the current daemon host can launch this protocol/runtime pair.
///
/// This is separate from [`AttachmentSpec`] parsing because Docker and
/// libkrun launch the Linux guest with the daemon's exact spec. A libkrun
/// guest must accept `fuse/libkrun` even though Linux cannot host libkrun.
#[must_use]
pub const fn attachment_pair_supported_on_current_host(
    protocol: AttachmentProtocol,
    runtime: AttachmentRuntime,
) -> bool {
    match (protocol, runtime) {
        (AttachmentProtocol::Nfs, AttachmentRuntime::Host)
        | (AttachmentProtocol::Fuse, AttachmentRuntime::Docker) => {
            cfg!(any(target_os = "linux", target_os = "macos"))
        },
        (AttachmentProtocol::Fuse, AttachmentRuntime::Host) => cfg!(target_os = "linux"),
        (AttachmentProtocol::Fuse, AttachmentRuntime::Libkrun) => {
            cfg!(all(target_os = "macos", target_arch = "aarch64"))
        },
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachmentSpecError {
    #[error("{protocol}/{runtime} is not a valid Attachment protocol/runtime pair")]
    UnsupportedPair {
        protocol: AttachmentProtocol,
        runtime: AttachmentRuntime,
    },
    #[error("host attachment location must be absolute: {}", .0.display())]
    HostLocationNotAbsolute(PathBuf),
    #[error("{runtime} owns its guest location; expected {ATTACHMENT_GUEST_LOCATION}, got {}", actual.display())]
    GuestLocation {
        runtime: AttachmentRuntime,
        actual: PathBuf,
    },
    #[error("host attachments cannot have runtime image references")]
    HostAssets,
    #[error("docker image is only valid for the docker runtime, not {runtime}")]
    DockerAssetOnOtherRuntime { runtime: AttachmentRuntime },
    #[error("libkrun guest image is only valid for the libkrun runtime, not {runtime}")]
    LibkrunAssetOnOtherRuntime { runtime: AttachmentRuntime },
    #[error("{field} cannot be empty")]
    EmptyAsset { field: &'static str },
}

/// Content version of an exact attachment specification.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AttachmentVersion([u8; 32]);

impl AttachmentVersion {
    #[must_use]
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for AttachmentVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for AttachmentVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "AttachmentVersion({self})")
    }
}

impl FromStr for AttachmentVersion {
    type Err = crate::ResourceDigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let digest: crate::ResourceDigest = value.parse()?;
        Ok(Self(*digest.as_bytes()))
    }
}

impl Serialize for AttachmentVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for AttachmentVersion {
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
            AttachmentSpec::new(
                AttachmentProtocol::Fuse,
                AttachmentRuntime::Host,
                PathBuf::from("relative"),
                None,
                None
            )
            .is_err()
        );
        assert!(
            AttachmentSpec::new(
                AttachmentProtocol::Nfs,
                AttachmentRuntime::Docker,
                PathBuf::from(ATTACHMENT_GUEST_LOCATION),
                None,
                None
            )
            .is_err()
        );
        assert!(
            AttachmentSpec::new(
                AttachmentProtocol::Fuse,
                AttachmentRuntime::Host,
                PathBuf::from("/tmp/omnifs"),
                Some("image".into()),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn exact_libkrun_spec_round_trips_inside_its_linux_guest() {
        let spec = AttachmentSpec::new(
            AttachmentProtocol::Fuse,
            AttachmentRuntime::Libkrun,
            ATTACHMENT_GUEST_LOCATION.into(),
            None,
            Some("guest.raw".into()),
        )
        .unwrap();
        let encoded = serde_json::to_vec(&spec).unwrap();
        assert_eq!(
            serde_json::from_slice::<AttachmentSpec>(&encoded).unwrap(),
            spec
        );
    }

    #[test]
    fn host_support_is_distinct_from_exact_spec_validity() {
        assert_eq!(
            attachment_pair_supported_on_current_host(
                AttachmentProtocol::Fuse,
                AttachmentRuntime::Libkrun
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
