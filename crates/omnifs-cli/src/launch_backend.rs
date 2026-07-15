//! Docker target types and process-role naming used by frontend lifecycle code.
//!
use std::fmt;

/// Whether this binary was produced by the release packaging lane
/// (`OMNIFS_RELEASE` set at compile time) or a local/dev build. Release
/// binaries default to the registry image for their version; dev binaries
/// default to the locally built dev image and never pull. Used by the
/// optional Docker-hosted FUSE frontend's image resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildChannel {
    Release,
    Dev,
}

impl BuildChannel {
    /// Why a missing registry-less image is never pulled. Only a dev binary
    /// defaults to a local image, so release errors must not call it a dev build.
    pub(crate) const fn pull_refusal_reason(self) -> &'static str {
        match self {
            Self::Dev => {
                "this omnifs binary is a dev build; it uses the locally built frontend image \
                 and never pulls from a registry"
            },
            Self::Release => {
                "registry-less image references are local build products; omnifs never pulls \
                 them from a registry"
            },
        }
    }

    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Release => "release",
        }
    }

    pub(crate) const fn version_suffix(self) -> &'static str {
        match self {
            Self::Dev => " (dev build)",
            Self::Release => "",
        }
    }
}

pub(crate) const BUILD_CHANNEL: BuildChannel = match option_env!("OMNIFS_RELEASE") {
    Some(_) => BuildChannel::Release,
    None => BuildChannel::Dev,
};

// The frontend container's mount path inside the container. The daemon
// resolves its own host-native mount point independently; this is the
// launcher's view of the frontend's guest boundary, used to build the
// `docker exec` working directory and wait for the mount. See
// `frontend_container.rs` and `commands/shell.rs`.
pub(crate) const GUEST_MOUNT: &str = "/omnifs";

/// How the omnifs process is running, which sets its default tracing level.
#[derive(Clone, Copy)]
pub(crate) enum ProcessRole {
    /// A foreground CLI invocation: stays quiet so ordinary commands are not
    /// noisy.
    Cli,
    /// A background daemon the CLI spawned: defaults louder so its startup
    /// diagnostics are captured in daemon.log rather than hidden.
    Daemon,
}

impl ProcessRole {
    /// The default `RUST_LOG` level for this run mode.
    pub(crate) const fn default_log_level(self) -> &'static str {
        match self {
            Self::Cli => "warn",
            Self::Daemon => "info",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ContainerName(String);

impl ContainerName {
    pub(crate) fn new(name: impl Into<String>) -> anyhow::Result<Self> {
        let name = name.into();
        validate_container_name(&name)?;
        Ok(Self(name))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn validate_container_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("container name must not be empty");
    }
    if name.len() > 64 {
        anyhow::bail!("container name must be at most 64 characters");
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("container name must not be empty");
    };
    if !first.is_ascii_alphanumeric() {
        anyhow::bail!("container name must start with an ASCII letter or digit");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
        anyhow::bail!("container name may only contain ASCII letters, digits, _, ., and -");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageRef(String);

impl ImageRef {
    pub(crate) fn new(image: impl Into<String>) -> anyhow::Result<Self> {
        let image = image.into();
        if image.trim().is_empty() {
            anyhow::bail!("image reference must not be empty");
        }
        Ok(Self(image))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// omnifs only pulls images whose reference names a registry host
/// (first path segment contains `.` or `:`, or is `localhost`). Bare
/// references like `omnifs-frontend:dev` are local build products: a Docker
/// Hub `library/omnifs-frontend` would never be a legitimate frontend image,
/// so treating registry-less references as local-only can't hide a real
/// image.
pub(crate) fn names_registry(image: &str) -> bool {
    // Per docker's reference grammar the registry, if present, is the first
    // path segment before the first `/`. A reference with no `/` (`omnifs-frontend:dev`)
    // has no registry component. A first segment is a registry iff it carries a
    // host marker: a dot, a port colon, or the literal `localhost`.
    match image.split_once('/') {
        None => false,
        Some((first, _)) => first.contains('.') || first.contains(':') || first == "localhost",
    }
}

/// A Docker container's name and image, addressed together. Built directly by
/// the frontend commands (`omnifs frontend enable|disable`); the daemon no
/// longer runs in a container, so there is no resolution chain here to guess
/// its identity from config or environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DockerTarget {
    container_name: ContainerName,
    image: ImageRef,
}

impl DockerTarget {
    pub(crate) fn new(container_name: String, image: String) -> anyhow::Result<Self> {
        Ok(Self {
            container_name: ContainerName::new(container_name)?,
            image: ImageRef::new(image)?,
        })
    }

    pub(crate) fn container_name(&self) -> &ContainerName {
        &self.container_name
    }

    pub(crate) fn image(&self) -> &ImageRef {
        &self.image
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_registry_table() {
        // (reference, expects a registry host, note)
        let cases = [
            ("omnifs-frontend:dev", false),
            ("omnifs-frontend:abc123-dev", false),
            // A Docker Hub org path is a pull target in docker semantics, but
            // NOT for us: its first segment carries no host marker.
            ("myorg/omnifs-frontend:1.0", false),
            ("ghcr.io/0xff-ai/omnifs-frontend:0.2.1", true),
            ("localhost:5000/omnifs-frontend:x", true),
            ("registry.local/omnifs-frontend", true),
        ];
        for (image, expected) in cases {
            assert_eq!(
                names_registry(image),
                expected,
                "names_registry({image:?}) should be {expected}"
            );
        }
    }
}
