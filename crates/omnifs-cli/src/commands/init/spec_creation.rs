use anyhow::{Context, anyhow};
use omnifs_caps::{Grants, Limits};
use omnifs_workspace::ids::ProviderRef;
use omnifs_workspace::mounts::{Name as MountName, ProviderMetadataInheritance, Spec};
use omnifs_workspace::provider::{
    ConfigField, ConfigMetadata, HostResourceBinding, ProviderManifest,
};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Default, Clone)]
pub(super) struct CreatedMountSpec {
    pub(super) config: Option<Value>,
    pub(super) capabilities: Option<Grants>,
    pub(super) limits: Option<Limits>,
}

pub(super) struct MountSpecCreator<'a> {
    reference: &'a ProviderRef,
    mount_name: &'a MountName,
    manifest: &'a ProviderManifest,
}

impl<'a> MountSpecCreator<'a> {
    pub(super) fn new(
        reference: &'a ProviderRef,
        mount_name: &'a MountName,
        manifest: &'a ProviderManifest,
    ) -> Self {
        Self {
            reference,
            mount_name,
            manifest,
        }
    }

    pub(super) fn create(&self, interactive: bool) -> anyhow::Result<CreatedMountSpec> {
        // Seed explicit grants from the manifest's declared needs. The manifest
        // never grants at runtime; the spec owns these grants from here on.
        let capabilities =
            (!self.manifest.capabilities.is_empty()).then(|| self.manifest.provider_capabilities());
        let limits = (!self.manifest.limits.is_empty()).then(|| self.manifest.provider_limits());
        let mut spec = Spec {
            provider: self.reference.clone(),
            mount: self.mount_name.to_string(),
            root_mount: false,
            revalidate: true,
            auth: None,
            capabilities: None,
            limits: None,
            config_raw: None,
        };
        spec.apply_provider_metadata(self.manifest, ProviderMetadataInheritance::config())
            .context("apply provider config defaults")?;
        let Some(mut config) = spec.config_raw else {
            return Ok(CreatedMountSpec {
                config: None,
                capabilities,
                limits,
            });
        };

        if interactive && let Some(config_metadata) = self.manifest.config.as_ref() {
            prompt_host_files(config_metadata, &mut config)?;
        }
        self.validate(&config)?;
        Ok(CreatedMountSpec {
            config: Some(config),
            capabilities,
            limits,
        })
    }

    pub(super) fn requires_prompt(&self) -> bool {
        let Some(config_metadata) = self.manifest.config.as_ref() else {
            return false;
        };
        config_metadata.requires_prompt()
    }

    fn validate(&self, config: &Value) -> anyhow::Result<()> {
        let config_metadata = self
            .manifest
            .config
            .as_ref()
            .ok_or_else(|| anyhow!("provider `{}` has no config metadata", self.manifest.id))?;
        config_metadata
            .validate_config(config)
            .map_err(|error| anyhow!("generated provider config failed validation: {error}"))
    }
}

/// Prompt for the host path of each field the provider marks as a host file and
/// write the chosen absolute path into the config. The matching preopen grant is
/// already seeded from the manifest's dynamic need; the host resolves the
/// preopen from this path at mount-start (guest == host), so init only collects
/// the value.
fn prompt_host_files(metadata: &ConfigMetadata, config: &mut Value) -> anyhow::Result<()> {
    let Some(config_obj) = config.as_object_mut() else {
        anyhow::bail!("generated config must be an object");
    };
    for (name, field) in metadata.host_resource_fields() {
        let Some(HostResourceBinding::File { .. }) = field.binding else {
            continue;
        };
        let host_path = prompt_host_file(name, field)?
            .canonicalize()
            .with_context(|| format!("canonicalize host file for `{name}`"))?;
        config_obj.insert(
            name.to_string(),
            Value::String(host_path.display().to_string()),
        );
    }
    Ok(())
}

fn prompt_host_file(name: &str, field: &ConfigField) -> anyhow::Result<PathBuf> {
    let description = field.description.as_deref().unwrap_or(name);
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
