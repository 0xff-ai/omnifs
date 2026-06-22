//! Provider contract pre-flight: classify and route contract deltas before a
//! reconcile runs.
//!
//! `omnifs up` calls `run_preflight` before launching or reconciling. For each
//! mount whose spec carries a `contract` block, the live provider contract is
//! derived and compared against the stamped block. The most severe change class
//! wins and determines the action:
//!
//! | Delta | Action |
//! |-------|--------|
//! | Identical | Nothing. |
//! | Additive config only | Auto-migrate: fill defaults, re-stamp, rewrite file. |
//! | Breaking config | Hard error: the user must re-init or upgrade manually. |
//! | Capability or auth | Hard error: requires explicit re-consent via `omnifs init`. |
//! | Provider removed | Hard error: the provider no longer ships. |
//!
//! Specs without a `contract` block (written before contract versioning) are
//! skipped: they proceed straight to reconcile, where a hard failure surfaces
//! as a `MountFailure` if the spec has drifted.

use std::path::Path;

use anyhow::Context as _;
use omnifs_mount::mounts::{Catalog, Spec};
use omnifs_mount::{Contract, ContractDelta};

use crate::session::MountConfig;

/// Run the contract pre-flight over all mounts, applying safe auto-migrations
/// in-place (additive config) and returning an error for any blocking change.
///
/// `mounts_dir` is the directory containing the per-mount JSON spec files.
/// `providers_dir` is where provider WASM and embedded manifests live.
/// `configs` is the list of mounts loaded by `Workspace::mounts()`.
///
/// On success the function returns the list of mount names that were
/// auto-migrated so the caller can log them.
pub(crate) fn run_preflight(
    mounts_dir: &Path,
    providers_dir: &Path,
    configs: &[MountConfig],
) -> anyhow::Result<Vec<String>> {
    let catalog = Catalog::new(mounts_dir, providers_dir);
    let mut migrated = Vec::new();

    for config in configs {
        let spec = &config.config;
        let Some(stamped) = &spec.contract else {
            // No contract block; skip the pre-flight for this mount.
            continue;
        };

        // Derive the live contract from the current provider manifest.
        let live = catalog
            .live_contract_for(spec)
            .with_context(|| format!("load provider manifest for mount `{}`", spec.mount))?;

        let Some(live) = live else {
            // Provider not found at all; this becomes a hard error at
            // materialize time, not a contract error.
            anstream::eprintln!(
                "warning: no provider manifest found for mount `{}`; \
                 skipping contract check (will fail at reconcile if provider is absent)",
                spec.mount
            );
            continue;
        };

        let delta = stamped.classify_against(&live);
        match delta {
            ContractDelta::Identical => {},

            ContractDelta::AdditiveConfig { added } => {
                // Auto-migrate: fill the new optional fields from the live manifest
                // defaults and re-stamp the contract block. Rewrite the spec file.
                // The contract block records field names and required-ness, not
                // values, so the defaults are sourced from the live manifest here.
                let defaults = catalog.live_field_defaults(spec).with_context(|| {
                    format!("load provider config defaults for mount `{}`", spec.mount)
                })?;
                let added: Vec<omnifs_mount::AddedField> = added
                    .into_iter()
                    .map(|mut field| {
                        if field.default.is_none() {
                            field.default = defaults.get(&field.name).cloned();
                        }
                        field
                    })
                    .collect();
                apply_additive_migration(&config.source, spec, &live, &added)?;
                migrated.push(spec.mount.clone());
                anstream::println!(
                    "✓ Mount `{}`: auto-migrated {} new optional field(s): {}",
                    spec.mount,
                    added.len(),
                    added
                        .iter()
                        .map(|f| f.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            },

            ContractDelta::BreakingConfig { changes } => {
                let descriptions: Vec<_> = changes
                    .iter()
                    .map(omnifs_mount::FieldChange::describe)
                    .collect();
                anyhow::bail!(
                    "mount `{}` has a breaking provider config change that requires manual \
                     update: {}\n\
                     Run `omnifs init {} --as {}` to re-initialise with the new schema, \
                     or edit the spec at {} manually.",
                    spec.mount,
                    descriptions.join(", "),
                    spec.provider,
                    spec.mount,
                    config.source.display(),
                );
            },

            ContractDelta::CapabilityOrAuth {
                capability_delta,
                auth_delta,
            } => {
                let mut parts = Vec::new();
                for cap_change in &capability_delta {
                    let direction = match cap_change.direction {
                        omnifs_mount::CapabilityDirection::Added => "added",
                        omnifs_mount::CapabilityDirection::Removed => "removed",
                    };
                    parts.push(format!(
                        "{} capability `{}` = `{}`",
                        direction, cap_change.kind.kind, cap_change.kind.value
                    ));
                }
                if let Some(auth) = auth_delta {
                    parts.push(format!(
                        "auth scheme changed from `{}` to `{}`",
                        auth.old.as_deref().unwrap_or("none"),
                        auth.new.as_deref().unwrap_or("none"),
                    ));
                }
                anyhow::bail!(
                    "mount `{}` requires re-consent: the provider's security surface changed.\n\
                     Changes: {}\n\
                     Run `omnifs init {} --as {}` to review and accept the new provider \
                     capabilities.",
                    spec.mount,
                    parts.join("; "),
                    spec.provider,
                    spec.mount,
                );
            },
        }
    }

    Ok(migrated)
}

/// Apply an additive-only migration: fill new optional fields with their
/// defaults from the live manifest and re-stamp the contract block.
///
/// Reads the current spec file as raw JSON, merges the new fields into the
/// `"config"` object (adding missing keys only, never overwriting), then
/// updates the `"contract"` block and writes back atomically.
fn apply_additive_migration(
    spec_path: &Path,
    spec: &Spec,
    live_contract: &Contract,
    added: &[omnifs_mount::AddedField],
) -> anyhow::Result<()> {
    // Read the raw JSON so we preserve all fields and formatting structure.
    let raw = std::fs::read_to_string(spec_path)
        .with_context(|| format!("read spec {}", spec_path.display()))?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse spec {}", spec_path.display()))?;

    // Merge new optional config fields: fill only absent keys.
    if let Some(obj) = doc.get_mut("config").and_then(|v| v.as_object_mut()) {
        for field in added {
            if !obj.contains_key(&field.name)
                && let Some(default) = &field.default
            {
                obj.insert(field.name.clone(), default.clone());
            }
        }
    } else if !added.is_empty() {
        // No "config" key yet; build one from the added fields' defaults.
        let mut config_map = serde_json::Map::new();
        for field in added {
            if let Some(default) = &field.default {
                config_map.insert(field.name.clone(), default.clone());
            }
        }
        if !config_map.is_empty() {
            doc["config"] = serde_json::Value::Object(config_map);
        }
    }

    // Re-stamp the contract block with the live contract.
    let contract_value = serde_json::to_value(live_contract).context("serialize live contract")?;
    doc["contract"] = contract_value;

    // Write back. Use pretty-print matching serde_json's default output, then
    // append a trailing newline (consistent with how `omnifs init` writes files).
    let new_content = format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).context("serialize updated spec")?
    );

    // Atomic write via temp + rename.
    let tmp_path = spec_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &new_content)
        .with_context(|| format!("write temp spec {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, spec_path)
        .with_context(|| format!("rename {} to {}", tmp_path.display(), spec_path.display()))?;

    let _ = spec; // used only for the mount name in caller messages
    Ok(())
}
