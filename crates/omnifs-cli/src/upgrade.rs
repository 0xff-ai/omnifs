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
//! | Capability, limit, or auth change | Warn and keep the pinned artifact; re-consent via `omnifs init`. |
//!
//! Problems are per-mount, never fatal to the whole command: a blocking upgrade
//! keeps the mount on its known-good pinned artifact (warn, skip), and a pinned
//! artifact that is no longer retained warns and lets the daemon report that one
//! mount as failed. This mirrors the daemon's per-mount reconcile, so one bad
//! mount cannot stop every other mount from coming up.

use std::path::Path;

use anyhow::Context as _;
use omnifs_workspace::ids::ProviderRef;
use omnifs_workspace::mounts::{ProviderMetadataInheritance, UpgradePlan};
use omnifs_workspace::mounts::{Registry, Spec};
use omnifs_workspace::provider::{Catalog, ProviderManifest};

use crate::session::MountConfig;

/// Run the upgrade check over all mounts, auto-migrating additive upgrades
/// in-place and warning (without blocking the other mounts) on any per-mount
/// problem. Returns the names of the mounts that were auto-migrated.
pub(crate) fn run_upgrade_check(
    providers_dir: &Path,
    configs: &[MountConfig],
) -> anyhow::Result<Vec<String>> {
    let catalog = Catalog::open(providers_dir);
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

        let pinned_manifest = pinned.manifest()?;
        let candidate_manifest = candidate.manifest()?;
        match UpgradePlan::diff(&pinned_manifest, &candidate_manifest) {
            UpgradePlan::Identical => {},
            UpgradePlan::AdditiveConfig { added } => {
                apply_additive_upgrade(
                    &config.source,
                    &candidate.reference(),
                    &candidate_manifest,
                    &added,
                )?;
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
                    .map(omnifs_workspace::mounts::FieldChange::describe)
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
            UpgradePlan::CapabilityLimitOrAuth {
                capabilities,
                limits,
                auth,
            } => {
                let mut parts = Vec::new();
                for change in &capabilities {
                    let direction = match change.direction {
                        omnifs_workspace::mounts::CapabilityDirection::Added => "added",
                        omnifs_workspace::mounts::CapabilityDirection::Removed => "removed",
                    };
                    parts.push(format!(
                        "{direction} capability `{}` = `{}`",
                        change.kind, change.value
                    ));
                }
                for change in &limits {
                    let direction = match change.direction {
                        omnifs_workspace::mounts::LimitDirection::Added => "added",
                        omnifs_workspace::mounts::LimitDirection::Removed => "removed",
                    };
                    parts.push(format!(
                        "{direction} limit `{}` = `{}`",
                        change.name, change.value
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
                     its access or runtime surface and needs re-consent.\nChanges: {}\n\
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
    manifest: &ProviderManifest,
    added: &[omnifs_workspace::mounts::AddedField],
) -> anyhow::Result<()> {
    let mut spec =
        Spec::from_file(spec_path).with_context(|| format!("read spec {}", spec_path.display()))?;

    spec.apply_provider_metadata(
        manifest,
        ProviderMetadataInheritance::additive_config(added),
    )?;
    spec.provider = reference.clone();

    let mounts_dir = spec_path.parent().unwrap_or_else(|| Path::new("."));
    Registry::load(mounts_dir)?.put(&spec)?;
    Ok(())
}
