use crate::sections::ProviderMetadataError;
use indexmap::IndexMap;
use omnifs_caps::PreopenMode;
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ConfigSchema {
    #[serde(rename = "type")]
    pub schema_type: ConfigSchemaType,
    #[serde(default)]
    pub properties: IndexMap<String, ConfigProperty>,
    /// JSON-Schema `required` array. A field can be required and still carry a
    /// default, so this is the authoritative required signal: default-presence
    /// only governs seeding (`defaults`), never required-ness.
    #[serde(default)]
    pub required: Vec<String>,
}

impl ConfigSchema {
    pub fn parse(schema: &Schema) -> Result<Self, ProviderMetadataError> {
        let config_schema: Self = serde_json::from_value(schema.as_value().clone())
            .map_err(|error| ProviderMetadataError::Validation(format!("configSchema: {error}")))?;
        if config_schema.schema_type != ConfigSchemaType::Object {
            return Err(ProviderMetadataError::Validation(
                "configSchema must be a top-level object schema".to_string(),
            ));
        }
        Ok(config_schema)
    }

    #[must_use]
    pub fn defaults(&self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        for (name, property) in &self.properties {
            if let Some(default) = &property.default {
                out.insert(name.clone(), default.clone());
            }
        }
        serde_json::Value::Object(out)
    }

    /// Whether `omnifs init` must prompt interactively for a value. Only a
    /// host-file field needs a prompt; the host resolves its preopen at
    /// mount-start from the path the user supplies.
    #[must_use]
    pub fn requires_prompt(&self) -> bool {
        self.resource_fields()
            .any(|(_, resource)| matches!(resource, HostResource::File { .. }))
    }

    /// The config fields declared as host-resource references, in declaration
    /// order. The host resolves each field's grant from its value at
    /// mount-start (a socket into the callout allowlist, a file into a WASI
    /// preopen).
    pub fn resource_fields(&self) -> impl Iterator<Item = (&str, HostResource)> {
        self.properties.iter().filter_map(|(name, property)| {
            property.resource.map(|resource| (name.as_str(), resource))
        })
    }

    /// The single field declared as `kind`, if any.
    #[must_use]
    pub fn resource_field(&self, kind: HostResourceKind) -> Option<&str> {
        self.resource_fields()
            .find(|(_, resource)| resource.kind() == kind)
            .map(|(name, _)| name)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConfigSchemaType {
    Object,
    String,
    Boolean,
    Integer,
    Number,
    Array,
    Null,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ConfigProperty {
    #[serde(default, rename = "type")]
    pub schema_type: Option<ConfigSchemaType>,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "x-omnifs-resource")]
    pub resource: Option<HostResource>,
}

/// Declares that a config field's value references a host resource the sandbox
/// must be granted. The provider also declares the matching capability as a
/// `dynamic` need; the host resolves the concrete grant from this field's value
/// at mount-start. One marker drives both the socket allowlist and the WASI
/// preopen, replacing the per-kind bespoke bindings.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum HostResource {
    /// A host file the provider opens through a preopened WASI directory. The
    /// host preopens the file's parent directory at the same path (guest ==
    /// host) with `mode`, so the provider opens the configured path unchanged.
    File {
        #[serde(default)]
        mode: PreopenMode,
    },
    /// A host unix socket the host issues provider callouts over. The value is a
    /// `unix://` endpoint resolved into the socket allowlist.
    Socket,
}

/// The kind discriminant of a [`HostResource`], for looking a field up by kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostResourceKind {
    File,
    Socket,
}

impl HostResource {
    #[must_use]
    pub fn kind(self) -> HostResourceKind {
        match self {
            Self::File { .. } => HostResourceKind::File,
            Self::Socket => HostResourceKind::Socket,
        }
    }
}
