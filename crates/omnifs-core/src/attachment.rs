//! Exact desired filesystem attachment specifications.

use crate::ResourceName;
use crate::fs::{self, Protocol, Runtime};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// An exact attachment configuration after daemon-owned normalization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentSpec {
    protocol: Protocol,
    runtime: Runtime,
    location: PathBuf,
    docker_image: Option<String>,
    libkrun_guest_image: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAttachmentSpec {
    protocol: Protocol,
    runtime: Runtime,
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
        protocol: Protocol,
        runtime: Runtime,
        location: PathBuf,
        docker_image: Option<String>,
        libkrun_guest_image: Option<String>,
    ) -> Result<Self, AttachmentSpecError> {
        if !supported_pair(protocol, runtime) {
            return Err(AttachmentSpecError::UnsupportedPair { protocol, runtime });
        }
        match runtime {
            Runtime::Host => {
                if !location.is_absolute() {
                    return Err(AttachmentSpecError::HostLocationNotAbsolute(location));
                }
                if docker_image.is_some() || libkrun_guest_image.is_some() {
                    return Err(AttachmentSpecError::HostAssets);
                }
            },
            Runtime::Docker => {
                if location != Path::new(fs::GUEST_LOCATION) {
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
            Runtime::Libkrun => {
                if location != Path::new(fs::GUEST_LOCATION) {
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

    #[must_use]
    pub fn docker_image(&self) -> Option<&str> {
        self.docker_image.as_deref()
    }

    #[must_use]
    pub fn libkrun_guest_image(&self) -> Option<&str> {
        self.libkrun_guest_image.as_deref()
    }

    /// Temporary conversion while VFS and runtime users still take `fs::Spec`.
    pub fn to_fs_spec(&self, name: &ResourceName) -> Result<fs::Spec, AttachmentSpecError> {
        fs::Spec::new(
            fs::Id::new(name.as_str()).map_err(AttachmentSpecError::FilesystemId)?,
            self.protocol,
            self.runtime,
            self.location.clone(),
        )
        .map_err(AttachmentSpecError::FilesystemSpec)
    }

    /// Temporary conversion of an old filesystem spec. Old specs carry no
    /// runtime asset reference, so the converted desired spec records `None`.
    pub fn from_fs_spec(spec: &fs::Spec) -> Result<Self, AttachmentSpecError> {
        Self::new(
            spec.protocol(),
            spec.runtime(),
            spec.location().to_path_buf(),
            None,
            None,
        )
    }
}

fn validate_asset(field: &'static str, value: Option<&str>) -> Result<(), AttachmentSpecError> {
    if value.is_some_and(str::is_empty) {
        return Err(AttachmentSpecError::EmptyAsset { field });
    }
    Ok(())
}

fn supported_pair(protocol: Protocol, runtime: Runtime) -> bool {
    match (protocol, runtime) {
        (Protocol::Nfs, Runtime::Host) | (Protocol::Fuse, Runtime::Docker) => {
            cfg!(any(target_os = "linux", target_os = "macos"))
        },
        (Protocol::Fuse, Runtime::Host) => cfg!(target_os = "linux"),
        (Protocol::Fuse, Runtime::Libkrun) => {
            cfg!(all(target_os = "macos", target_arch = "aarch64"))
        },
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttachmentSpecError {
    #[error("{protocol}/{runtime} is not supported on this platform")]
    UnsupportedPair {
        protocol: Protocol,
        runtime: Runtime,
    },
    #[error("host attachment location must be absolute: {}", .0.display())]
    HostLocationNotAbsolute(PathBuf),
    #[error("{runtime} owns its guest location; expected {}, got {}", fs::GUEST_LOCATION, actual.display())]
    GuestLocation { runtime: Runtime, actual: PathBuf },
    #[error("host attachments cannot have runtime image references")]
    HostAssets,
    #[error("docker image is only valid for the docker runtime, not {runtime}")]
    DockerAssetOnOtherRuntime { runtime: Runtime },
    #[error("libkrun guest image is only valid for the libkrun runtime, not {runtime}")]
    LibkrunAssetOnOtherRuntime { runtime: Runtime },
    #[error("{field} cannot be empty")]
    EmptyAsset { field: &'static str },
    #[error("invalid temporary filesystem identity: {0}")]
    FilesystemId(fs::IdError),
    #[error("invalid temporary filesystem spec: {0}")]
    FilesystemSpec(fs::SpecError),
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

    fn supported() -> (Protocol, Runtime) {
        if cfg!(target_os = "linux") {
            (Protocol::Fuse, Runtime::Host)
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            (Protocol::Fuse, Runtime::Libkrun)
        } else {
            (Protocol::Nfs, Runtime::Host)
        }
    }

    #[test]
    fn rejects_invalid_locations_assets_and_pairs() {
        let (protocol, runtime) = supported();
        assert!(
            AttachmentSpec::new(protocol, runtime, PathBuf::from("relative"), None, None).is_err()
        );
        assert!(
            AttachmentSpec::new(
                Protocol::Nfs,
                Runtime::Docker,
                PathBuf::from(fs::GUEST_LOCATION),
                None,
                None
            )
            .is_err()
        );
        assert!(
            AttachmentSpec::new(
                protocol,
                Runtime::Host,
                PathBuf::from("/tmp/omnifs"),
                Some("image".into()),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn round_trips_through_temporary_fs_spec() {
        let (protocol, runtime) = supported();
        let location = if runtime == Runtime::Host {
            PathBuf::from("/tmp/omnifs")
        } else {
            PathBuf::from(fs::GUEST_LOCATION)
        };
        let spec = AttachmentSpec::new(protocol, runtime, location, None, None).unwrap();
        let old = spec
            .to_fs_spec(&ResourceName::new("local").unwrap())
            .unwrap();
        assert_eq!(AttachmentSpec::from_fs_spec(&old).unwrap(), spec);
    }
}
