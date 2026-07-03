use crate::provider::sections::ProviderMetadataError;
use omnifs_caps::PreopenMode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<ConfigField>,
}

impl ConfigMetadata {
    pub fn validate(&self) -> Result<(), ProviderMetadataError> {
        validate_fields("config", &self.fields)
    }

    #[must_use]
    pub fn defaults(&self) -> serde_json::Value {
        let fields = self
            .fields
            .iter()
            .filter_map(|field| {
                field
                    .default
                    .as_ref()
                    .map(|default| (field.name.clone(), default.clone()))
            })
            .collect();
        serde_json::Value::Object(fields)
    }

    /// Whether `omnifs init` must prompt interactively for a value. Only a
    /// host-file field needs a prompt; the host resolves its preopen at
    /// mount-start from the path the user supplies.
    #[must_use]
    pub fn requires_prompt(&self) -> bool {
        self.host_resource_fields()
            .any(|(_, field)| matches!(field.binding, Some(HostResourceBinding::File { .. })))
    }

    /// The config fields bound to host resources, in declaration order. The
    /// host resolves each field's grant from its value at mount-start.
    pub fn host_resource_fields(&self) -> impl Iterator<Item = (&str, &ConfigField)> {
        self.fields
            .iter()
            .filter(|field| field.binding.is_some())
            .map(|field| (field.name.as_str(), field))
    }

    /// The config field bound to the host socket, if any.
    #[must_use]
    pub fn host_socket_field(&self) -> Option<&str> {
        self.host_resource_fields()
            .find(|(_, field)| matches!(field.binding, Some(HostResourceBinding::Socket)))
            .map(|(name, _)| name)
    }

    pub fn validate_config(&self, config: &serde_json::Value) -> Result<(), ConfigError> {
        let mut errors = Vec::new();
        validate_object_value(&self.fields, config, "", &mut errors);
        errors
            .is_empty()
            .then_some(())
            .ok_or_else(|| ConfigError(errors.join("; ")))
    }
}

fn validate_fields(path: &str, fields: &[ConfigField]) -> Result<(), ProviderMetadataError> {
    let mut names = HashSet::new();
    for field in fields {
        if field.name.is_empty() {
            return Err(ProviderMetadataError::Validation(format!(
                "{path}: config field name must not be empty"
            )));
        }
        if !names.insert(field.name.as_str()) {
            return Err(ProviderMetadataError::Validation(format!(
                "{path}: duplicate config field `{}`",
                field.name
            )));
        }
        field.validate(path)?;
    }
    Ok(())
}

fn validate_object_value(
    fields: &[ConfigField],
    value: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    let Some(object) = value.as_object() else {
        errors.push(format!(
            "{}must be an object",
            path_prefix(path).unwrap_or_default()
        ));
        return;
    };
    for key in object.keys() {
        if !fields.iter().any(|field| field.name == *key) {
            errors.push(format!("unknown field `{}`{}", key, at_path(path)));
        }
    }
    for field in fields {
        match object.get(&field.name) {
            Some(value) => {
                let field_path = child_path(path, &field.name);
                field.value_type.validate_value(value, &field_path, errors);
            },
            None if field.required => {
                errors.push(format!(
                    "missing required field `{}`{}",
                    field.name,
                    at_path(path)
                ));
            },
            None => {},
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigField {
    pub name: String,
    #[serde(rename = "type")]
    pub value_type: ConfigType,
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<HostResourceBinding>,
}

impl ConfigField {
    fn validate(&self, path: &str) -> Result<(), ProviderMetadataError> {
        if self.binding.is_some() && !matches!(self.value_type, ConfigType::String) {
            return Err(ProviderMetadataError::Validation(format!(
                "{path}.{}: host-resource bindings are only valid on string fields",
                self.name
            )));
        }
        self.value_type
            .validate_metadata(&format!("{path}.{}", self.name))?;
        if let Some(default) = &self.default {
            let mut errors = Vec::new();
            self.value_type
                .validate_value(default, &format!("{path}.{}", self.name), &mut errors);
            if !errors.is_empty() {
                return Err(ProviderMetadataError::Validation(format!(
                    "invalid default for config field `{}`: {}",
                    self.name,
                    errors.join("; ")
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ConfigType {
    String,
    Boolean,
    Integer,
    Array { items: Box<ConfigType> },
    Map { values: Box<ConfigType> },
    Object { fields: Vec<ConfigField> },
}

impl ConfigType {
    fn validate_metadata(&self, path: &str) -> Result<(), ProviderMetadataError> {
        match self {
            Self::Array { items } => items.validate_metadata(&format!("{path}[]")),
            Self::Map { values } => values.validate_metadata(&format!("{path}.*")),
            Self::Object { fields } => validate_fields(path, fields),
            Self::String | Self::Boolean | Self::Integer => Ok(()),
        }
    }

    fn validate_value(&self, value: &serde_json::Value, path: &str, errors: &mut Vec<String>) {
        match self {
            Self::String if !value.is_string() => errors.push(expected(path, "string")),
            Self::Boolean if !value.is_boolean() => errors.push(expected(path, "boolean")),
            Self::Integer if !is_integer(value) => errors.push(expected(path, "integer")),
            Self::Array { items } => {
                let Some(values) = value.as_array() else {
                    errors.push(expected(path, "array"));
                    return;
                };
                for (index, value) in values.iter().enumerate() {
                    items.validate_value(value, &format!("{path}[{index}]"), errors);
                }
            },
            Self::Map { values } => {
                let Some(object) = value.as_object() else {
                    errors.push(expected(path, "object"));
                    return;
                };
                for (key, value) in object {
                    values.validate_value(value, &child_path(path, key), errors);
                }
            },
            Self::Object { fields } => {
                validate_object_value(fields, value, path, errors);
            },
            Self::String | Self::Boolean | Self::Integer => {},
        }
    }
}

/// Binds a config field's string value to a host resource the sandbox must be
/// granted. The provider also declares the matching capability as a `dynamic`
/// need; the host resolves the concrete grant from this field's value at
/// mount-start.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum HostResourceBinding {
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

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(String);

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

fn is_integer(value: &serde_json::Value) -> bool {
    value.as_i64().is_some() || value.as_u64().is_some()
}

fn expected(path: &str, expected: &str) -> String {
    format!(
        "{}must be {expected}",
        path_prefix(path).unwrap_or_default()
    )
}

fn path_prefix(path: &str) -> Option<String> {
    (!path.is_empty()).then(|| format!("`{path}` "))
}

fn at_path(path: &str) -> String {
    path_prefix(path).map_or_else(String::new, |path| format!(" at {path}"))
}

fn child_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}.{child}")
    }
}
