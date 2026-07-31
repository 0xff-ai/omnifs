use anyhow::{Context, anyhow};
use omnifs_provider::{
    ConfigField, ConfigMetadata, ConfigType, HostResourceBinding, ProviderManifest,
    is_hostname_only,
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
        prompt_config_fields(config_metadata, &mut config, output, false)?;
        if let Some(field) = manifest.dynamic_domain_field() {
            prompt_domains(field, &mut config, output)?;
        }
    }
    validate_config(manifest, &config)?;
    Ok(Some(config))
}

/// Revisit one provider's config using its current strict value as defaults.
///
/// The manifest remains the schema owner. This helper only collects values
/// and validates the complete object before the daemon planner sees it.
pub(crate) fn update_config(
    manifest: &ProviderManifest,
    current: &Value,
    output: &Output,
) -> anyhow::Result<Value> {
    let Some(config_metadata) = manifest.config.as_ref() else {
        anyhow::ensure!(
            current.as_object().is_some_and(serde_json::Map::is_empty),
            "provider `{}` has no config metadata",
            manifest.id
        );
        return Ok(current.clone());
    };
    let mut config = current.clone();
    prompt_config_fields(config_metadata, &mut config, output, true)?;
    if let Some(field) = manifest.dynamic_domain_field() {
        prompt_domains_with_default(field, &mut config, output)?;
    }
    validate_config(manifest, &config)?;
    Ok(config)
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
fn prompt_config_fields(
    metadata: &ConfigMetadata,
    config: &mut Value,
    output: &Output,
    revisit: bool,
) -> anyhow::Result<()> {
    let Some(config_obj) = config.as_object_mut() else {
        anyhow::bail!("generated config must be an object");
    };
    for field in &metadata.fields {
        if field.name == "domains"
            && matches!(
                &field.value_type,
                ConfigType::Array { items } if matches!(items.as_ref(), ConfigType::String)
            )
        {
            continue;
        }
        let current = config_obj.get(&field.name).cloned();
        match field.binding {
            Some(HostResourceBinding::File { .. }) => {
                let host_path = prompt_host_file(
                    &field.name,
                    field,
                    current.as_ref().and_then(Value::as_str),
                    output,
                )?
                .canonicalize()
                .with_context(|| format!("canonicalize host file for `{}`", field.name))?;
                config_obj.insert(
                    field.name.clone(),
                    Value::String(host_path.display().to_string()),
                );
            },
            Some(HostResourceBinding::Socket) => {
                if revisit || current.is_none() {
                    let endpoint = prompt_value(field, current.as_ref(), output)?;
                    validate_socket_endpoint(&field.name, &endpoint)?;
                    config_obj.insert(field.name.clone(), endpoint);
                } else if let Some(endpoint) = current.as_ref() {
                    validate_socket_endpoint(&field.name, endpoint)?;
                }
            },
            None if field.required && current.is_none() => {
                let value = prompt_value(field, None, output)?;
                config_obj.insert(field.name.clone(), value);
            },
            None if revisit && current.is_some() => {
                let value = prompt_value(field, current.as_ref(), output)?;
                config_obj.insert(field.name.clone(), value);
            },
            None => {},
        }
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

fn prompt_domains_with_default(
    field: &str,
    config: &mut Value,
    output: &Output,
) -> anyhow::Result<()> {
    let Some(config_obj) = config.as_object_mut() else {
        anyhow::bail!("generated config must be an object");
    };
    let default = config_obj
        .get(field)
        .and_then(Value::as_array)
        .map(|domains| {
            domains
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let raw =
        crate::ui::prompt::Text::new("Domains this mount may fetch (space- or comma-separated)")
            .with_default(default)
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

fn prompt_host_file(
    name: &str,
    field: &ConfigField,
    current: Option<&str>,
    output: &Output,
) -> anyhow::Result<PathBuf> {
    let description = field.description.as_deref().unwrap_or(name);
    let mut prompt = crate::ui::prompt::Text::new(description);
    if let Some(current) = current {
        prompt = prompt.with_default(current);
    }
    let raw = prompt.ask_with_output(output)?;
    let path = crate::ui::input_path(raw.trim());
    if !path.is_file() {
        anyhow::bail!("{} is not a readable file", path.display());
    }
    Ok(path)
}

fn prompt_value(
    field: &ConfigField,
    current: Option<&Value>,
    output: &Output,
) -> anyhow::Result<Value> {
    let question = field.description.as_deref().unwrap_or(&field.name);
    let default = current
        .map(value_as_prompt_text)
        .or_else(|| field.default.as_ref().map(value_as_prompt_text));
    let mut prompt = crate::ui::prompt::Text::new(question);
    if let Some(default) = default {
        prompt = prompt.with_default(default);
    }
    let raw = prompt.ask_with_output(output)?;
    parse_prompt_value(&field.value_type, &raw)
        .with_context(|| format!("parse config field `{}`", field.name))
}

fn value_as_prompt_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn parse_prompt_value(value_type: &ConfigType, raw: &str) -> anyhow::Result<Value> {
    match value_type {
        ConfigType::String => Ok(Value::String(raw.to_owned())),
        ConfigType::Boolean => raw
            .parse::<bool>()
            .map(Value::Bool)
            .context("expected `true` or `false`"),
        ConfigType::Integer => raw
            .parse::<i64>()
            .map(serde_json::Number::from)
            .map(Value::Number)
            .context("expected an integer"),
        ConfigType::Array { .. } | ConfigType::Map { .. } | ConfigType::Object { .. } => {
            serde_json::from_str(raw).context("expected JSON")
        },
    }
}

fn validate_socket_endpoint(name: &str, value: &Value) -> anyhow::Result<()> {
    let endpoint = value
        .as_str()
        .ok_or_else(|| anyhow!("host socket config `{name}` must be a string"))?;
    let path = endpoint
        .strip_prefix("unix://")
        .ok_or_else(|| anyhow!("host socket config `{name}` must start with `unix://`"))?;
    anyhow::ensure!(
        PathBuf::from(path).is_absolute(),
        "host socket config `{name}` must name an absolute unix socket path"
    );
    Ok(())
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
