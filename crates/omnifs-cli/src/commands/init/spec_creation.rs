use anyhow::{Context, anyhow};
use omnifs_caps::Grants;
use omnifs_provider::{ConfigProperty, ConfigSchema, HostResource, ProviderManifest};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Default, Clone)]
pub(super) struct CreatedMountSpec {
    pub(super) config: Option<Value>,
    pub(super) capabilities: Option<Grants>,
}

pub(super) struct MountSpecCreator<'a> {
    manifest: &'a ProviderManifest,
}

impl<'a> MountSpecCreator<'a> {
    pub(super) fn new(manifest: &'a ProviderManifest) -> Self {
        Self { manifest }
    }

    pub(super) fn create(&self, interactive: bool) -> anyhow::Result<CreatedMountSpec> {
        // Seed explicit grants from the manifest's declared needs. The manifest
        // never grants at runtime; the spec owns these grants from here on.
        let capabilities =
            (!self.manifest.capabilities.is_empty()).then(|| self.manifest.provider_capabilities());
        let Some(schema) = self.manifest.config_schema.as_ref() else {
            return Ok(CreatedMountSpec {
                config: None,
                capabilities,
            });
        };
        let schema = ConfigSchema::parse(schema)?;
        let mut config = schema.defaults();
        if interactive {
            prompt_host_files(&schema, &mut config)?;
        }
        self.validate(&config)?;
        Ok(CreatedMountSpec {
            config: Some(config),
            capabilities,
        })
    }

    pub(super) fn requires_prompt(&self) -> bool {
        let Some(schema) = self.manifest.config_schema.as_ref() else {
            return false;
        };
        ConfigSchema::parse(schema).is_ok_and(|schema| schema.requires_prompt())
    }

    fn validate(&self, config: &Value) -> anyhow::Result<()> {
        let schema = self
            .manifest
            .config_schema
            .as_ref()
            .ok_or_else(|| anyhow!("provider `{}` has no configSchema", self.manifest.id))?;
        omnifs_provider::validate_config(schema.as_value(), config)
            .map_err(|error| anyhow!("generated provider config failed schema validation: {error}"))
    }
}

/// Prompt for the host path of each field the provider marks as a host file and
/// write the chosen absolute path into the config. The matching preopen grant is
/// already seeded from the manifest's dynamic need; the host resolves the
/// preopen from this path at mount-start (guest == host), so init only collects
/// the value.
fn prompt_host_files(schema: &ConfigSchema, config: &mut Value) -> anyhow::Result<()> {
    let Some(config_obj) = config.as_object_mut() else {
        anyhow::bail!("generated config must be an object");
    };
    for (name, property) in &schema.properties {
        let Some(HostResource::File { .. }) = property.resource else {
            continue;
        };
        let host_path = prompt_host_file(name, property)?
            .canonicalize()
            .with_context(|| format!("canonicalize host file for `{name}`"))?;
        config_obj.insert(name.clone(), Value::String(host_path.display().to_string()));
    }
    Ok(())
}

fn prompt_host_file(name: &str, property: &ConfigProperty) -> anyhow::Result<PathBuf> {
    let description = property.description.as_deref().unwrap_or(name);
    let raw = inquire::Text::new(description)
        .prompt()
        .map_err(|e| anyhow!("prompt error: {e}"))?;
    let path = expand_tilde_path(raw.trim());
    if !path.is_file() {
        anyhow::bail!("{} is not a readable file", path.display());
    }
    Ok(path)
}

fn expand_tilde_path(raw: &str) -> PathBuf {
    if let Some(stripped) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(stripped);
    }
    PathBuf::from(raw)
}
