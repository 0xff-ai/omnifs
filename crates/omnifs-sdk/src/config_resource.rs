//! Config metadata and host-resource config fields.
//!
//! The `#[config]` macro implements [`ProvidesConfigMetadata`] directly from the
//! config struct's field syntax. A field whose value names a host resource is
//! still declared ergonomically as [`HostFile`] or [`HostSocket`], but the
//! manifest records it as a string field with an Omnifs host-resource binding
//! that the host resolves at mount-start.

use std::ops::Deref;
use std::path::Path;

/// Provides the config metadata of a provider's config type for the embedded
/// manifest. The `#[config]` macro implements it; [`NoConfig`] has no config
/// metadata.
///
/// [`NoConfig`]: crate::NoConfig
pub trait ProvidesConfigMetadata {
    /// The config metadata, or `None` when the provider takes no config.
    const METADATA: Option<ConfigMetadata>;
}

impl ProvidesConfigMetadata for crate::NoConfig {
    const METADATA: Option<ConfigMetadata> = None;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigMetadata {
    pub fields: &'static [ConfigField],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigField {
    pub name: &'static str,
    pub value_type: ConfigType,
    pub required: bool,
    pub default: Option<DefaultValue>,
    pub description: Option<&'static str>,
    pub binding: Option<HostResourceBinding>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigType {
    String,
    Boolean,
    Integer,
    Array(&'static ConfigType),
    Map(&'static ConfigType),
    Object(&'static [ConfigField]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultValue {
    String(&'static str),
    Boolean(bool),
    Integer(i64),
}

/// Binds a config field's string value to a host resource the sandbox must be
/// granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostResourceBinding {
    File,
    Socket,
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
