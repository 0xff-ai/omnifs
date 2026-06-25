//! Config-schema harvesting and host-resource config fields.
//!
//! A provider's `manifest_json()` export splices the config schema (which the
//! proc-macro cannot evaluate) in through [`ProvidesConfigSchema`]. A config
//! field whose value is a host resource the sandbox must be granted is declared
//! by its *type* ([`HostFile`] / [`HostSocket`]); the field's `JsonSchema` emits
//! the `x-omnifs-resource` marker the host resolves at mount-start (see
//! `omnifs_provider::HostResource`).

use std::ops::Deref;
use std::path::Path;

/// Provides the JSON Schema of a provider's config type for the embedded
/// manifest. The `#[config]` macro implements it via `schema_for!`; [`NoConfig`]
/// has no schema.
///
/// [`NoConfig`]: crate::NoConfig
pub trait ProvidesConfigSchema {
    /// The config JSON Schema, or `None` when the provider takes no config.
    fn config_schema() -> Option<serde_json::Value>;
}

impl ProvidesConfigSchema for crate::NoConfig {
    fn config_schema() -> Option<serde_json::Value> {
        None
    }
}

/// A config field whose value is a host file the provider opens through a
/// preopened WASI directory. Declares the field as `x-omnifs-resource: { kind:
/// file, mode: ro }`; the host preopens the file's parent directory at the same
/// path at mount-start, so the provider opens the value unchanged.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(transparent)]
pub struct HostFile(pub String);

/// A config field whose value is a `unix://` host socket the host issues
/// provider callouts over. Declares the field as `x-omnifs-resource: { kind:
/// socket }`; the host resolves it into the callout allowlist at mount-start.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(transparent)]
pub struct HostSocket(pub String);

macro_rules! host_resource_field {
    ($ty:ty, $name:literal, $marker:tt) => {
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
        impl schemars::JsonSchema for $ty {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                $name.into()
            }
            // Inline at the use site so the `x-omnifs-resource` marker lands
            // directly on the config property; a `$ref` into `$defs` would hide
            // it from the host's per-field resource lookup.
            fn inline_schema() -> bool {
                true
            }
            fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({
                    "type": "string",
                    "x-omnifs-resource": $marker,
                })
            }
        }
    };
}

host_resource_field!(HostFile, "HostFile", { "kind": "file", "mode": "ro" });
host_resource_field!(HostSocket, "HostSocket", { "kind": "socket" });
