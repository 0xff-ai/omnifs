use anyhow::{Context, anyhow};
use omnifs_provider::{
    ConfigField, ConfigMetadata, HostResourceBinding, ProviderManifest, is_hostname_only,
};
use serde_json::Value;
use std::path::PathBuf;

use crate::ui::output::Output;

pub(crate) fn create_config(
    manifest: &ProviderManifest,
    output: &Output,
    interactive: bool,
) -> anyhow::Result<Option<Value>> {
    let Some(config_metadata) = manifest.config.as_ref() else {
        return Ok(None);
    };
    let mut config = config_metadata.defaults();
    if interactive {
        prompt_host_files(config_metadata, &mut config, output)?;
        if let Some(field) = manifest.dynamic_domain_field() {
            prompt_domains(field, &mut config, output)?;
        }
    }
    validate_config(manifest, &config)?;
    Ok(Some(config))
}

pub(crate) fn validate_config(manifest: &ProviderManifest, config: &Value) -> anyhow::Result<()> {
    let config_metadata = manifest
        .config
        .as_ref()
        .ok_or_else(|| anyhow!("provider `{}` has no config metadata", manifest.id))?;
    config_metadata
        .validate_config(config)
        .map_err(|error| anyhow!("provider config failed validation: {error}"))?;
    if let Some(field) = manifest.dynamic_domain_field() {
        validate_dynamic_domains(config, field)?;
    }
    Ok(())
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
    use super::{parse_domain_list, validate_config};
    use omnifs_provider::ProviderManifest;

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
            "refreshIntervalSecs": 0,
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
        assert!(
            validate_config(
                &manifest,
                &serde_json::json!({"domains": ["API.Example.COM"]})
            )
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
                validate_config(&manifest, &value).is_err(),
                "expected {value} to fail"
            );
        }
    }
}
