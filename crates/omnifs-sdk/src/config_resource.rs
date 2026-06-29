//! Host-resource config fields and the config-metadata bridge.
//!
//! The `#[config]` macro implements [`ProvidesConfigMetadata`] directly from the
//! config struct's field syntax. A field whose value names a host resource is
//! still declared ergonomically as [`HostFile`] or [`HostSocket`], but the
//! manifest records it as a string field with an omnifs host-resource binding
//! that the host resolves at mount-start.
//!
//! The config-metadata wire types live in `omnifs-provider` (re-exported from
//! the crate root for non-wasm targets); the harvester serializes them verbatim.
//! Only the runtime field types ([`HostFile`], [`HostSocket`]) are referenced
//! inside the wasm guest.

use std::ops::Deref;
use std::path::Path;

/// Provides the config metadata of a provider's config type for the embedded
/// manifest. The `#[config]` macro implements it; [`NoConfig`] has no config
/// metadata.
///
/// Host-only: a provider's metadata is constructed and serialized by the
/// build-time harvester, never inside the wasm guest.
///
/// [`NoConfig`]: crate::NoConfig
#[cfg(not(target_arch = "wasm32"))]
pub trait ProvidesConfigMetadata {
    /// The config metadata, or `None` when the provider takes no config.
    fn metadata() -> Option<crate::ConfigMetadata>;
}

#[cfg(not(target_arch = "wasm32"))]
impl ProvidesConfigMetadata for crate::NoConfig {
    fn metadata() -> Option<crate::ConfigMetadata> {
        None
    }
}

/// A config field whose value is a host file the provider opens through a
/// preopened WASI directory. The manifest records this as a string field with a
/// host-file binding; the host preopens the file's parent directory at the same
/// path at mount-start, so the provider opens the value unchanged.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(transparent)]
pub struct HostFile(pub String);

/// A config field whose value is a `unix://` host socket the host issues
/// provider callouts over. The manifest records this as a string field with a
/// host-socket binding; the host resolves it into the callout allowlist at
/// mount-start.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(transparent)]
pub struct HostSocket(pub String);

macro_rules! host_resource_field {
    ($ty:ty) => {
        impl $ty {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl From<$ty> for String {
            fn from(value: $ty) -> Self {
                value.0
            }
        }
        impl Deref for $ty {
            type Target = str;
            fn deref(&self) -> &str {
                &self.0
            }
        }
        impl AsRef<str> for $ty {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
        impl AsRef<Path> for $ty {
            fn as_ref(&self) -> &Path {
                Path::new(&self.0)
            }
        }
        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

host_resource_field!(HostFile);
host_resource_field!(HostSocket);
