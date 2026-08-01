//! Shared image identity for filesystem runtimes. Build-channel identity
//! itself is a binary-level fact owned by `omnifs-bootstrap`, so version.rs
//! can keep working daemon-free; this module re-exports it for the
//! resolvers below that still need it.

use std::fmt;

pub(crate) use omnifs_bootstrap::{BUILD_CHANNEL, BuildChannel};

/// The explicit > env > config > default precedence chain shared by every
/// runtime image resolver: an explicit override wins if given, else the
/// named environment variable, else the profile's configured value, else the
/// build-channel default.
pub(crate) fn resolve_image_reference(
    explicit: Option<String>,
    env_var: &str,
    configured: Option<&str>,
    default: &'static str,
) -> String {
    explicit
        .or_else(|| std::env::var(env_var).ok())
        .or_else(|| configured.map(str::to_owned))
        .unwrap_or_else(|| default.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef(String);

impl ImageRef {
    pub fn new(image: impl Into<String>) -> anyhow::Result<Self> {
        let image = image.into();
        if image.trim().is_empty() {
            anyhow::bail!("image reference must not be empty");
        }
        Ok(Self(image))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this reference names a registry host. Bare references such as
    /// `omnifs-filesystem:dev` are local build products.
    pub fn has_registry(&self) -> bool {
        match self.0.split_once('/') {
            None => false,
            Some((first, _)) => first.contains('.') || first.contains(':') || first == "localhost",
        }
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::ImageRef;

    #[test]
    fn names_registry_table() {
        let cases = [
            ("omnifs-filesystem:dev", false),
            ("omnifs-filesystem:abc123-dev", false),
            ("myorg/omnifs-filesystem:1.0", false),
            ("ghcr.io/0xff-ai/omnifs-filesystem:0.2.1", true),
            ("localhost:5000/omnifs-filesystem:x", true),
            ("registry.local/omnifs-filesystem", true),
        ];
        for (image, expected) in cases {
            let image = ImageRef::new(image).unwrap();
            assert_eq!(
                image.has_registry(),
                expected,
                "ImageRef::has_registry({image:?}) should be {expected}"
            );
        }
    }
}
