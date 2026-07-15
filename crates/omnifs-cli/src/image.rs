//! Shared image identity and build-channel policy for frontend runtimes.

use std::fmt;

/// Whether this binary was produced by the release packaging lane
/// (`OMNIFS_RELEASE` set at compile time) or a local/dev build. Release
/// binaries default to the registry image for their version; dev binaries
/// default to the locally built dev image and never pull.
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

/// Return whether an image reference names a registry host. Bare references
/// such as `omnifs-frontend:dev` are local build products.
pub(crate) fn names_registry(image: &str) -> bool {
    match image.split_once('/') {
        None => false,
        Some((first, _)) => first.contains('.') || first.contains(':') || first == "localhost",
    }
}

#[cfg(test)]
mod tests {
    use super::names_registry;

    #[test]
    fn names_registry_table() {
        let cases = [
            ("omnifs-frontend:dev", false),
            ("omnifs-frontend:abc123-dev", false),
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
