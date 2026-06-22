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

    #[must_use]
    pub fn requires_prompt(&self) -> bool {
        self.properties.values().any(|property| {
            property
                .init
                .as_ref()
                .is_some_and(|hint| hint.input.is_some())
        })
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
    #[serde(default, rename = "x-omnifs-init")]
    pub init: Option<InitHint>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InitHint {
    #[serde(default)]
    pub input: Option<InitInput>,
    #[serde(default)]
    pub guest_dir: Option<String>,
    #[serde(default)]
    pub preopen_mode: PreopenMode,
    #[serde(default)]
    pub preopen_strategy: PreopenStrategy,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InitInput {
    HostFile,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PreopenStrategy {
    #[default]
    Append,
    Replace,
}
