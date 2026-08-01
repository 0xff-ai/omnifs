//! `omnifs setup`: one coherent desired-state quick start.

use std::collections::BTreeSet;
use std::time::Instant;

use anyhow::{Context as _, Result};
use omnifs_api::{
    ApplyReceipt, FilesystemDefinition, MountResourceDefinition, ProgressSnapshot,
    ProviderDefinition, ProviderMetadata, ResourceDeclarations, ResourceDefinition, ResourceLimits,
};
use omnifs_core::{ResourceKind, ResourceName};
use omnifs_provider::ProviderManifest;
use serde::Serialize;

use crate::commands::{daemon_start, filesystem, resource_flow};
use crate::error::ExitCode;
use crate::provider_catalog::{
    align_provider_catalog_rows, needs_no_sign_in, provider_catalog_row,
};
use crate::provider_resolver::ProviderResolver;
use crate::rpc::RpcClient;
use crate::ui::output::{Output, ResultVerdict};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupResult {
    providers: Vec<ResourceName>,
    mounts: Vec<ResourceName>,
    filesystem: Option<ResourceName>,
    receipt: Option<ApplyReceipt>,
    snapshot: Option<ProgressSnapshot>,
}

struct CatalogEntry {
    manifest: ProviderManifest,
    mounted: bool,
}

struct PreparedSetup {
    declarations: ResourceDeclarations,
    provider_names: Vec<ResourceName>,
    mount_names: Vec<ResourceName>,
    filesystem_name: Option<ResourceName>,
}

pub async fn run(output: Output) -> Result<ExitCode> {
    daemon_start::start(&output).await?;
    let started = Instant::now();
    let rpc = RpcClient::resolve()?;
    let prepared = prepare_setup(&rpc, &output).await?;
    let plan = rpc.plan_resources(&prepared.declarations).await?;
    if !output.quiet() {
        output.plan(&resource_flow::plan_preview(
            "Setup desired resources",
            &plan,
        ));
    }
    let changed = plan
        .changes
        .iter()
        .any(|change| change.action != omnifs_api::ResourceChangeAction::Unchanged);
    let (receipt, snapshot) = if changed {
        match crate::ui::consent::Decision::resolve(
            output.prompt_mode(),
            false,
            "Apply setup plan?",
            "--yes",
            &output,
        )? {
            crate::ui::consent::Decision::Apply => {},
            crate::ui::consent::Decision::DryRun => {
                unreachable!("setup has no dry-run mode")
            },
        }
        let applied = match resource_flow::apply_plan_and_wait(
            &rpc,
            &output,
            plan,
            prepared.declarations,
            Vec::new(),
        )
        .await
        {
            Ok(applied) => applied,
            Err(error) => return resource_flow::finish_resource_error(&output, error),
        };
        (Some(applied.receipt), Some(applied.snapshot))
    } else {
        let snapshot = resource_flow::wait_for_revision(&rpc, plan.base_revision, &output).await?;
        (None, Some(snapshot))
    };

    let result = SetupResult {
        providers: prepared.provider_names,
        mounts: prepared.mount_names,
        filesystem: prepared.filesystem_name,
        receipt,
        snapshot,
    };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else if !output.quiet() {
        if !result.mounts.is_empty() {
            output.report(format!(
                "mounted: {}\n",
                result
                    .mounts
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(name) = &result.filesystem {
            output.report(format!("Filesystem {name} ready\n"));
        }
        output.outro(format!("All set in {}s.", started.elapsed().as_secs()));
    }
    Ok(ExitCode::Success)
}

async fn prepare_setup(rpc: &RpcClient, output: &Output) -> Result<PreparedSetup> {
    let current = rpc.resources().await?;
    let embedded = rpc.list_embedded_providers().await?;
    let mounted = mounted_provider_names(&current.resources);
    let entries = catalog_entries(&embedded, &mounted);

    render_catalog(&entries, output);

    let offered = entries
        .iter()
        .filter(|entry| !entry.mounted && needs_no_sign_in(&entry.manifest))
        .map(|entry| entry.manifest.id.clone())
        .collect::<Vec<_>>();
    let add_providers = if offered.is_empty() {
        false
    } else {
        let question = format!(
            "Mount the {} that need no sign-in ({})?",
            count(offered.len(), "service"),
            offered.join(", ")
        );
        crate::ui::consent::resolve_confirm(output.prompt_mode(), question, true, false, output)?
    };

    let recommended = filesystem::recommended_definition(rpc).await?;
    let add_filesystem = if let Some(definition) = recommended.as_ref() {
        !has_filesystem_pair(&current.resources, definition)
            && crate::ui::consent::resolve_confirm(
                output.prompt_mode(),
                format!(
                    "Create the recommended Filesystem ({} {} at {})?",
                    definition.spec.protocol(),
                    definition.spec.runtime(),
                    definition.spec.location().display()
                ),
                true,
                false,
                output,
            )?
    } else {
        false
    };

    let mut resources = current.resources;
    let mut provider_names = Vec::new();
    let mut mount_names = Vec::new();
    if add_providers {
        for selector in &offered {
            let resolved = ProviderResolver::new(rpc)
                .resolve(selector, &embedded)
                .await
                .with_context(|| format!("prepare quick-start provider `{selector}`"))?;
            let provider_name = ResourceName::new(resolved.manifest.id.clone())?;
            let mount_name = unique_name(
                &resources,
                ResourceKind::Mount,
                &resolved.manifest.default_mount,
            )?;
            upsert(
                &mut resources,
                ResourceDefinition::Provider(ProviderDefinition {
                    name: provider_name.clone(),
                    artifact: resolved.reference.id,
                }),
            );
            upsert(
                &mut resources,
                ResourceDefinition::Mount(MountResourceDefinition {
                    name: mount_name.clone(),
                    provider: provider_name.clone(),
                    credential: None,
                    config: serde_json::json!({}),
                    limits: manifest_limits(&resolved.manifest),
                }),
            );
            provider_names.push(provider_name);
            mount_names.push(mount_name);
        }
    }

    let filesystem_name = if add_filesystem {
        let definition = recommended.context("recommended Filesystem disappeared")?;
        let name = definition.name.clone();
        upsert(&mut resources, ResourceDefinition::Filesystem(definition));
        Some(name)
    } else {
        None
    };

    Ok(PreparedSetup {
        declarations: ResourceDeclarations {
            api_version: omnifs_api::API_VERSION.to_owned(),
            resources,
        },
        provider_names,
        mount_names,
        filesystem_name,
    })
}

fn mounted_provider_names(resources: &[ResourceDefinition]) -> BTreeSet<ResourceName> {
    resources
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Mount(definition) => Some(definition.provider.clone()),
            _ => None,
        })
        .collect()
}

fn catalog_entries(
    embedded: &[ProviderMetadata],
    mounted: &BTreeSet<ResourceName>,
) -> Vec<CatalogEntry> {
    let mut entries = embedded
        .iter()
        .filter_map(|entry| {
            let manifest = ProviderManifest::from_bytes(&entry.manifest).ok()?;
            let name = ResourceName::new(manifest.id.clone()).ok()?;
            Some(CatalogEntry {
                manifest,
                mounted: mounted.contains(&name),
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
    entries
}

fn render_catalog(entries: &[CatalogEntry], output: &Output) {
    if entries.is_empty() || output.is_structured() || output.quiet() {
        return;
    }
    let rows = entries
        .iter()
        .map(|entry| provider_catalog_row(&entry.manifest))
        .collect::<Vec<_>>();
    output.heading("Providers you can mount:");
    for (entry, mut line) in entries.iter().zip(align_provider_catalog_rows(&rows)) {
        if entry.mounted {
            line.push_str("  mounted");
        }
        output.narrate(line);
    }
}

fn has_filesystem_pair(resources: &[ResourceDefinition], candidate: &FilesystemDefinition) -> bool {
    resources.iter().any(|resource| {
        matches!(
            resource,
            ResourceDefinition::Filesystem(definition)
                if definition.spec.protocol() == candidate.spec.protocol()
                    && definition.spec.runtime() == candidate.spec.runtime()
        )
    })
}

fn unique_name(
    resources: &[ResourceDefinition],
    kind: ResourceKind,
    preferred: &str,
) -> Result<ResourceName> {
    for suffix in 1_u32.. {
        let value = if suffix == 1 {
            preferred.to_owned()
        } else {
            format!("{preferred}-{suffix}")
        };
        let candidate = ResourceName::new(value)?;
        if resources
            .iter()
            .all(|resource| resource.kind() != kind || resource.name() != &candidate)
        {
            return Ok(candidate);
        }
    }
    unreachable!("u32 name suffix space exhausted")
}

fn upsert(resources: &mut Vec<ResourceDefinition>, definition: ResourceDefinition) {
    let key = definition.key();
    resources.retain(|resource| resource.key() != key);
    resources.push(definition);
}

fn manifest_limits(manifest: &ProviderManifest) -> Option<ResourceLimits> {
    (!manifest.limits.is_empty()).then(|| ResourceLimits {
        max_memory_mb: manifest
            .limits
            .max_memory_mb
            .as_ref()
            .map(|limit| limit.value),
        max_fetch_blob_bytes: manifest
            .limits
            .max_fetch_blob_bytes
            .as_ref()
            .map(|limit| limit.value),
    })
}

fn count(value: usize, noun: &str) -> String {
    if value == 1 {
        format!("1 {noun}")
    } else {
        format!("{value} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_core::{
        FILESYSTEM_GUEST_LOCATION, FilesystemProtocol, FilesystemRuntime, FilesystemSpec,
    };
    use omnifs_provider::LimitDeclarations;
    use std::path::PathBuf;

    fn filesystem(
        name: &str,
        protocol: FilesystemProtocol,
        runtime: FilesystemRuntime,
    ) -> FilesystemDefinition {
        FilesystemDefinition {
            name: ResourceName::new(name).unwrap(),
            spec: FilesystemSpec::new(
                protocol,
                runtime,
                if runtime == FilesystemRuntime::Host {
                    PathBuf::from("/tmp/omnifs")
                } else {
                    PathBuf::from(FILESYSTEM_GUEST_LOCATION)
                },
                None,
                None,
            )
            .unwrap(),
        }
    }

    #[test]
    fn filesystem_offer_is_pair_based() {
        let existing = filesystem("one", FilesystemProtocol::Nfs, FilesystemRuntime::Host);
        let candidate = filesystem("two", FilesystemProtocol::Nfs, FilesystemRuntime::Host);
        assert!(has_filesystem_pair(
            &[ResourceDefinition::Filesystem(existing)],
            &candidate
        ));
    }

    #[test]
    fn unique_mount_names_do_not_replace_existing_desired_state() {
        let resources = vec![ResourceDefinition::Mount(MountResourceDefinition {
            name: ResourceName::new("dns").unwrap(),
            provider: ResourceName::new("dns").unwrap(),
            credential: None,
            config: serde_json::json!({}),
            limits: None,
        })];
        assert_eq!(
            unique_name(&resources, ResourceKind::Mount, "dns")
                .unwrap()
                .as_str(),
            "dns-2"
        );
    }

    #[test]
    fn empty_manifest_limits_stay_absent() {
        let manifest = ProviderManifest {
            id: "dns".to_owned(),
            display_name: "DNS".to_owned(),
            description: None,
            provider: "dns.wasm".to_owned(),
            default_mount: "dns".to_owned(),
            version: None,
            wit_package: None,
            sdk_version: None,
            refresh_interval_secs: 0,
            capabilities: Vec::new(),
            limits: LimitDeclarations::default(),
            auth: None,
            config: None,
        };
        assert_eq!(manifest_limits(&manifest), None);
    }
}
