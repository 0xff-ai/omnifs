//! Provider upgrade check: classify and route a newer provider artifact before
//! a reconcile runs.
//!
//! `omnifs up` calls `run_upgrade_check`. For each mount it loads the pinned
//! artifact's manifest and, when a newer artifact for the same provider name is
//! installed, diffs the two manifests and routes the change:
//!
//! | Plan | Action |
//! |------|--------|
//! | Identical / no newer artifact | Nothing. |
//! | Additive optional config | Auto-migrate: fill defaults, repin, rewrite the spec file. |
//! | Breaking config | Hard error: re-init with the new schema. |
//! | Capability or auth change | Warn and keep the pinned artifact; re-consent via `omnifs init`. |
//!
//! Problems are per-mount, never fatal to the whole command: a blocking upgrade
//! keeps the mount on its known-good pinned artifact (warn, skip), and a pinned
//! artifact that is no longer retained warns and lets the daemon report that one
//! mount as failed. This mirrors the daemon's per-mount reconcile, so one bad
//! mount cannot stop every other mount from coming up.

use std::path::Path;

use anyhow::Context as _;
use omnifs_core::ProviderRef;
use omnifs_mount::UpgradePlan;
use omnifs_mount::mounts::Catalog;

use crate::session::MountConfig;

/// Run the upgrade check over all mounts, auto-migrating additive upgrades
/// in-place and warning (without blocking the other mounts) on any per-mount
/// problem. Returns the names of the mounts that were auto-migrated.
pub(crate) fn run_upgrade_check(
    mounts_dir: &Path,
    providers_dir: &Path,
    configs: &[MountConfig],
) -> anyhow::Result<Vec<String>> {
    let catalog = Catalog::new(mounts_dir, providers_dir);
    let mut migrated = Vec::new();

    for config in configs {
        let spec = &config.config;
        let mount = spec.mount.as_str();
        let name = &spec.provider.meta.name;

        let Some(pinned) = catalog.get(&spec.provider.id)? else {
            anstream::eprintln!(
                "warning: mount `{mount}` provider artifact is missing (pinned id {id}); \
                 the daemon will report this mount as failed. \
                 Run `omnifs mounts add {mount}` or `omnifs init {name}` to re-pin it.",
                id = spec.provider.id,
            );
            continue;
        };

        let Some(candidate) = catalog.latest_by_name(name)? else {
            continue;
        };
        if candidate.id == spec.provider.id {
            continue;
        }

        match UpgradePlan::diff(&pinned.manifest()?, &candidate.manifest()?) {
            UpgradePlan::Identical => {},
            UpgradePlan::AdditiveConfig { added } => {
                apply_additive_upgrade(&config.source, &candidate.reference(), &added)?;
                migrated.push(mount.to_owned());
                anstream::println!(
                    "✓ Mount `{mount}`: upgraded `{name}` and filled {} new optional field(s): {}",
                    added.len(),
                    added
                        .iter()
                        .map(|field| field.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            },
            UpgradePlan::BreakingConfig { changes } => {
                let descriptions: Vec<_> = changes
                    .iter()
                    .map(omnifs_mount::FieldChange::describe)
                    .collect();
                anstream::eprintln!(
                    "warning: mount `{mount}` keeps its pinned `{name}`: a newer artifact has a \
                     breaking config change ({}) and cannot be adopted automatically.\n\
                     Run `omnifs init {name} --as {mount}` to re-initialise with the new schema, \
                     or edit {} manually.",
                    descriptions.join(", "),
                    config.source.display(),
                );
            },
            UpgradePlan::CapabilityOrAuth { caps, auth } => {
                let mut parts = Vec::new();
                for change in &caps {
                    let direction = match change.direction {
                        omnifs_mount::CapabilityDirection::Added => "added",
                        omnifs_mount::CapabilityDirection::Removed => "removed",
                    };
                    parts.push(format!(
                        "{direction} capability `{}` = `{}`",
                        change.kind, change.value
                    ));
                }
                if let Some(auth) = auth {
                    parts.push(format!(
                        "auth scheme changed from `{}` to `{}`",
                        auth.old.as_deref().unwrap_or("none"),
                        auth.new.as_deref().unwrap_or("none"),
                    ));
                }
                anstream::eprintln!(
                    "warning: mount `{mount}` keeps its pinned `{name}`: a newer artifact changed \
                     its security surface and needs re-consent.\nChanges: {}\n\
                     Run `omnifs init {name} --as {mount}` to review and accept the new surface.",
                    parts.join("; "),
                );
            },
        }
    }
    Ok(migrated)
}

/// Fill new optional config fields and repin the provider to `reference`,
/// rewriting the spec file atomically. Existing config keys are never
/// overwritten.
fn apply_additive_upgrade(
    spec_path: &Path,
    reference: &ProviderRef,
    added: &[omnifs_mount::AddedField],
) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(spec_path)
        .with_context(|| format!("read spec {}", spec_path.display()))?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse spec {}", spec_path.display()))?;

    if let Some(obj) = doc
        .get_mut("config")
        .and_then(|value| value.as_object_mut())
    {
        for field in added {
            if !obj.contains_key(&field.name)
                && let Some(default) = &field.default
            {
                obj.insert(field.name.clone(), default.clone());
            }
        }
    } else {
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

    doc["provider"] = serde_json::to_value(reference).context("serialize provider ref")?;

    let new_content = format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).context("serialize spec")?
    );
    let tmp_path = spec_path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &new_content)
        .with_context(|| format!("write temp spec {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, spec_path)
        .with_context(|| format!("rename {} to {}", tmp_path.display(), spec_path.display()))
}
