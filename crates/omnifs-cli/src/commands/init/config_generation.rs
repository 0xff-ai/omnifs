use anyhow::{Context, anyhow};
use omnifs_caps::{Grant, Grants, PreopenedPath};
use omnifs_provider::{
    ConfigProperty, ConfigSchema, InitHint, InitInput, PreopenStrategy, ProviderManifest,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub(super) struct GeneratedMountConfig {
    pub(super) config: Option<Value>,
    pub(super) capabilities: Option<Grants>,
}

pub(super) struct MountConfigGenerator<'a> {
    manifest: &'a ProviderManifest,
}

impl<'a> MountConfigGenerator<'a> {
    pub(super) fn new(manifest: &'a ProviderManifest) -> Self {
        Self { manifest }
    }

    pub(super) fn generate(&self, interactive: bool) -> anyhow::Result<GeneratedMountConfig> {
        // Seed explicit grants from the manifest's declared needs. The manifest
        // never grants at runtime; the spec owns these grants from here on.
        let mut capabilities =
            (!self.manifest.capabilities.is_empty()).then(|| self.manifest.provider_capabilities());
        let Some(schema) = self.manifest.config_schema.as_ref() else {
            return Ok(GeneratedMountConfig {
                config: None,
                capabilities,
            });
        };
        let schema = ConfigSchema::parse(schema)?;
        let mut config = schema.defaults();
        if interactive {
            self.prompt_fields(&schema, &mut config, &mut capabilities)?;
        }
        self.validate(&config)?;
        Ok(GeneratedMountConfig {
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

    pub(super) fn apply_host_file_hint(
        &self,
        field_name: &str,
        hint: &InitHint,
        host_path: &Path,
        config: &mut Value,
        capabilities: &mut Option<Grants>,
    ) -> anyhow::Result<()> {
        let host_path = host_path
            .canonicalize()
            .with_context(|| format!("canonicalize {}", host_path.display()))?;
        let guest_dir = hint
            .guest_dir
            .as_deref()
            .ok_or_else(|| anyhow!("x-omnifs-init for `{field_name}` must set guestDir"))?;
        if !guest_dir.starts_with('/') {
            anyhow::bail!("x-omnifs-init guestDir for `{field_name}` must be absolute");
        }
        let host_parent = host_path
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", host_path.display()))?;
        let host_parent = host_parent
            .to_str()
            .ok_or_else(|| anyhow!("{} is not valid UTF-8", host_parent.display()))?;
        let file_name = host_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("{} has no UTF-8 file name", host_path.display()))?;
        let guest_path = format!("{}/{}", guest_dir.trim_end_matches('/'), file_name);

        let manifest_caps =
            (!self.manifest.capabilities.is_empty()).then(|| self.manifest.provider_capabilities());
        let mut caps = capabilities.take().or(manifest_caps).unwrap_or_default();
        let preopen = PreopenedPath {
            host: host_parent.to_string(),
            guest: guest_dir.to_string(),
            mode: hint.preopen_mode,
        };
        match hint.preopen_strategy {
            // Replace swaps in this field's preopen, dropping any preopen the
            // manifest seeded as a placeholder. `prompt_fields` enforces at most
            // one Replace hint per provider, so this never clobbers another
            // field's Replace.
            PreopenStrategy::Replace => {
                caps.preopened_paths = Some(Grant::Literal(vec![preopen]));
            },
            PreopenStrategy::Append => {
                let grant = caps
                    .preopened_paths
                    .get_or_insert_with(|| Grant::Literal(Vec::new()));
                let Grant::Literal(preopens) = grant else {
                    anyhow::bail!(
                        "x-omnifs-init for `{field_name}` cannot append a preopen to a dynamic grant"
                    );
                };
                if let Some(existing) = preopens
                    .iter()
                    .find(|existing| existing.guest == preopen.guest)
                {
                    if existing.host != preopen.host || existing.mode != preopen.mode {
                        anyhow::bail!(
                            "x-omnifs-init for `{field_name}` conflicts with an existing preopen for `{}`",
                            preopen.guest
                        );
                    }
                } else {
                    preopens.push(preopen);
                }
            },
        }
        let Some(config_obj) = config.as_object_mut() else {
            anyhow::bail!("generated config must be an object");
        };
        config_obj.insert(field_name.to_string(), Value::String(guest_path));
        *capabilities = Some(caps);
        Ok(())
    }

    fn prompt_fields(
        &self,
        schema: &ConfigSchema,
        config: &mut Value,
        capabilities: &mut Option<Grants>,
    ) -> anyhow::Result<()> {
        let mut replace_used = false;
        for (name, property) in &schema.properties {
            let Some(hint) = property.init.as_ref() else {
                continue;
            };
            match hint.input {
                Some(InitInput::HostFile) => {
                    if hint.preopen_strategy == PreopenStrategy::Replace {
                        if replace_used {
                            anyhow::bail!(
                                "x-omnifs-init for `{name}` uses preopenStrategy: replace, but a previous field already used Replace; at most one Replace hint per provider is allowed"
                            );
                        }
                        replace_used = true;
                    }
                    let host_path = prompt_host_file(name, property)?;
                    self.apply_host_file_hint(name, hint, &host_path, config, capabilities)?;
                },
                None => anyhow::bail!("x-omnifs-init for `{name}` must set input"),
            }
        }
        Ok(())
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
