//! Host-resource config fields and compile-time config metadata bytes.
//!
//! The `#[config]` macro emits the static JSON field dialect used by the
//! provider metadata section. A field whose value names a host resource is
//! still declared ergonomically as [`HostFile`] or [`HostSocket`], while the
//! manifest records it as a string field with an omnifs host-resource binding
//! that the host resolves at mount-start.

use std::ops::Deref;
use std::path::Path;

/// Compile-time JSON bytes for a provider config's field dialect.
///
/// `JSON` is the UTF-8 JSON array of config fields, and `LEN` is its exact byte
/// length. The provider macro wraps those fields in the manifest's `config`
/// object while assembling the metadata custom section at compile time.
///
/// [`NoConfig`]: crate::NoConfig
pub trait ConfigMetadataBytes {
    const LEN: usize;
    const JSON: &'static [u8];
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
