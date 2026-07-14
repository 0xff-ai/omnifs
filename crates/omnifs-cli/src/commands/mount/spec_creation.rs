use anyhow::{Context, anyhow};
use omnifs_caps::Limits;
use omnifs_workspace::ids::ProviderRef;
use omnifs_workspace::mounts::{Name as MountName, ProviderMetadataInheritance, Spec};
use omnifs_workspace::provider::{
    ConfigField, ConfigMetadata, HostResourceBinding, ProviderManifest, is_hostname_only,
};
use serde_json::Value;
use std::path::PathBuf;

use crate::ui::output::Output;

#[derive(Debug, Default, Clone)]
pub(crate) struct CreatedMountSpec {
    pub(crate) config: Option<Value>,
    pub(crate) limits: Option<Limits>,
}

pub(crate) struct MountSpecCreator<'a> {
    reference: &'a ProviderRef,
    mount_name: &'a MountName,
    manifest: &'a ProviderManifest,
}

impl<'a> MountSpecCreator<'a> {
    pub(crate) fn new(
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

    /// Spec skeleton for a caller that supplies the whole config through an
    /// override flag: pinned manifest needs and limits, no generated config,
    /// no prompts, no default-config validation (the override is validated
    /// where it is applied).
    pub(crate) fn create_for_config_override(&self) -> CreatedMountSpec {
        CreatedMountSpec {
            config: None,
            limits: (!self.manifest.limits.is_empty()).then(|| self.manifest.provider_limits()),
        }
    }

    pub(crate) fn create(
        &self,
        output: &Output,
        interactive: bool,
    ) -> anyhow::Result<CreatedMountSpec> {
        let limits = (!self.manifest.limits.is_empty()).then(|| self.manifest.provider_limits());
        let mut spec = Spec {
            provider: self.reference.clone(),
            mount: self.mount_name.to_string(),
            revalidate: true,
            auth: None,
            limits: None,
            config_raw: None,
        };
        spec.apply_provider_metadata(self.manifest, ProviderMetadataInheritance::config())
            .context("apply provider config defaults")?;
        let Some(mut config) = spec.config_raw else {
            return Ok(CreatedMountSpec {
                config: None,
                limits,
            });
        };

        if interactive {
            if let Some(config_metadata) = self.manifest.config.as_ref() {
                prompt_host_files(config_metadata, &mut config, output)?;
            }
            if let Some(field) = self.manifest.dynamic_domain_field() {
                prompt_domains(field, &mut config, output)?;
            }
        }
        self.validate(&config)?;
        Ok(CreatedMountSpec {
            config: Some(config),
            limits,
        })
    }

    pub(crate) fn requires_prompt(&self) -> bool {
        self.manifest.requires_mount_input()
    }

    pub(crate) fn validate(&self, config: &Value) -> anyhow::Result<()> {
        let config_metadata = self
            .manifest
            .config
            .as_ref()
            .ok_or_else(|| anyhow!("provider `{}` has no config metadata", self.manifest.id))?;
        config_metadata
            .validate_config(config)
            .map_err(|error| anyhow!("provider config failed validation: {error}"))?;
        if let Some(field) = self.manifest.dynamic_domain_field() {
            validate_dynamic_domains(config, field)?;
        }
        Ok(())
    }
}

/// Prompt for the host path of each field the provider marks as a host file and
/// write the chosen absolute path into the config. Startup pairs the bound field
/// with the manifest's dynamic need and resolves the exact preopen from this
/// path (guest == host), so init only collects the value.
fn prompt_host_files(
    metadata: &ConfigMetadata,
    config: &mut Value,
    output: &Output,
) -> anyhow::Result<()> {
    let Some(config_obj) = config.as_object_mut() else {
        anyhow::bail!("generated config must be an object");
    };
    for (name, field) in metadata.host_resource_fields() {
        let Some(HostResourceBinding::File { .. }) = field.binding else {
            continue;
        };
        let host_path = prompt_host_file(name, field, output)?
            .canonicalize()
            .with_context(|| format!("canonicalize host file for `{name}`"))?;
        config_obj.insert(
            name.to_string(),
            Value::String(host_path.display().to_string()),
        );
    }
    Ok(())
}

/// Collect the dynamic-domain allowlist interactively and write it into the
/// `domains` config field the provider reads. Startup resolves the dynamic
/// domain authority from exactly these hostnames, so an empty list is refused
/// here rather than producing a mount whose authority can never
/// resolve. A list supplied another way (an inherited default) is left as-is
/// when already non-empty.
fn prompt_domains(field: &str, config: &mut Value, output: &Output) -> anyhow::Result<()> {
    let Some(config_obj) = config.as_object_mut() else {
        anyhow::bail!("generated config must be an object");
    };
    if config_obj
        .get(field)
        .and_then(Value::as_array)
        .is_some_and(|domains| !domains.is_empty())
    {
        return Ok(());
    }
    let raw = crate::ui::prompt::Text::new(
        "Domains this mount may fetch (space- or comma-separated, e.g. example.com docs.rs)",
    )
    .ask_with_output(output)?;
    let domains = parse_domain_list(&raw)?;
    if domains.is_empty() {
        anyhow::bail!("at least one domain is required to fetch anything");
    }
    config_obj.insert(
        field.to_string(),
        Value::Array(domains.into_iter().map(Value::String).collect()),
    );
    Ok(())
}

/// Split a user-entered domain list on whitespace and commas and validate each
/// entry as a bare hostname. Matches the dynamic-domain authority's runtime
/// allowlist rules (no scheme, port, path, or wildcard), so the collected value
/// cannot widen the authority beyond what the provider legitimately fetches.
fn parse_domain_list(raw: &str) -> anyhow::Result<Vec<String>> {
    let mut domains = Vec::new();
    for token in raw.split(|c: char| c.is_whitespace() || c == ',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if !is_hostname_only(token) {
            anyhow::bail!(
                "invalid domain `{token}`: use bare hostnames only, without scheme, port, path, or wildcard"
            );
        }
        domains.push(token.to_string());
    }
    Ok(domains)
}

fn validate_dynamic_domains(config: &Value, field: &str) -> anyhow::Result<()> {
    let Some(domains) = config.get(field).and_then(Value::as_array) else {
        anyhow::bail!("dynamic domain config `{field}` must be a non-empty array of hostnames");
    };
    if domains.is_empty() {
        anyhow::bail!("dynamic domain config `{field}` must be a non-empty array of hostnames");
    }
    for domain in domains {
        let Some(domain) = domain.as_str() else {
            anyhow::bail!("dynamic domain config `{field}` must contain only bare hostnames");
        };
        if !is_hostname_only(domain) {
            anyhow::bail!(
                "invalid domain `{domain}` in `{field}`: use bare hostnames only, without scheme, port, path, or wildcard"
            );
        }
    }
    Ok(())
}

fn prompt_host_file(name: &str, field: &ConfigField, output: &Output) -> anyhow::Result<PathBuf> {
    let description = field.description.as_deref().unwrap_or(name);
    let raw = crate::ui::prompt::Text::new(description).ask_with_output(output)?;
    let path = crate::ui::input_path(raw.trim());
    if !path.is_file() {
        anyhow::bail!("{} is not a readable file", path.display());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{MountSpecCreator, parse_domain_list};
    use omnifs_workspace::ids::{ProviderId, ProviderMeta, ProviderName, ProviderRef};
    use omnifs_workspace::mounts::Name as MountName;
    use omnifs_workspace::provider::ProviderManifest;

    #[test]
    fn parses_and_validates_a_domain_list() {
        let domains = parse_domain_list("example.com, docs.rs  api.github.com").unwrap();
        assert_eq!(domains, ["example.com", "docs.rs", "api.github.com"]);
    }

    #[test]
    fn empty_input_yields_no_domains() {
        assert!(parse_domain_list("   ,  ").unwrap().is_empty());
    }

    #[test]
    fn rejects_non_bare_hostnames() {
        // A dynamic domain authority must not be widened by scheme, path, port, or
        // wildcard entries; each of these is refused.
        for bad in [
            "https://example.com",
            "example.com/path",
            "example.com:443",
            "*",
        ] {
            assert!(parse_domain_list(bad).is_err(), "`{bad}` must be rejected");
        }
    }

    #[test]
    fn accepts_uppercase_hostnames() {
        assert_eq!(
            parse_domain_list("API.Example.COM").unwrap(),
            ["API.Example.COM"]
        );
    }

    #[test]
    fn validate_rejects_invalid_dynamic_domain_config() {
        let manifest: ProviderManifest = serde_json::from_value(serde_json::json!({
            "id": "web",
            "displayName": "Web",
            "provider": "web.wasm",
            "defaultMount": "web",
            "capabilities": [{
                "kind": "domain",
                "value": "resolved from config",
                "why": "fetch configured domains",
                "dynamic": true
            }],
            "config": {"fields": [{
                "name": "domains",
                "type": {"kind": "array", "items": {"kind": "string"}}
            }]}
        }))
        .unwrap();
        let reference = ProviderRef {
            id: ProviderId::from_wasm_bytes(b"web"),
            meta: ProviderMeta {
                name: ProviderName::new("web").unwrap(),
                version: None,
            },
        };
        let mount_name = MountName::try_from("web").unwrap();
        let creator = MountSpecCreator::new(&reference, &mount_name, &manifest);

        assert!(
            creator
                .validate(&serde_json::json!({"domains": ["API.Example.COM"]}))
                .is_ok()
        );
        for value in [
            serde_json::json!({"domains": []}),
            serde_json::json!({"domains": [""]}),
            serde_json::json!({"domains": ["example.com/path"]}),
            serde_json::json!({"domains": ["example.com:443"]}),
            serde_json::json!({"domains": ["*"]}),
        ] {
            assert!(
                creator.validate(&value).is_err(),
                "expected {value} to fail"
            );
        }
    }
}
