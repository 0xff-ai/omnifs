//! Interactive Provider resource porcelain and read-only inventory.

use std::fmt::{self, Write as _};

use clap::{Args, Subcommand};
use omnifs_api::{ProviderDefinition, ResourceDefinition, ResourcePhase};
use omnifs_core::{ResourceKind, ResourceName};
use omnifs_provider::ProviderManifest;
use serde::Serialize;

use crate::commands::{daemon_start, resource_flow};
use crate::error::ExitCode;
use crate::provider_resolver::ProviderResolver;
use crate::rpc::RpcClient;
use crate::ui::output::{Output, ResultVerdict};

#[derive(Args, Debug)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub command: ProviderCommand,
}

#[derive(Subcommand, Debug)]
pub enum ProviderCommand {
    /// Add an embedded or local Wasm provider resource.
    Add,
    /// List desired Provider resources.
    Ls,
    /// Show one desired Provider resource.
    Show { name: ResourceName },
    /// Remove one Provider resource.
    Rm { name: ResourceName },
}

#[derive(Debug, Clone, Copy)]
enum SourceChoice {
    Embedded,
    Local,
}

impl fmt::Display for SourceChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Embedded => formatter.write_str("embedded provider"),
            Self::Local => formatter.write_str("local Wasm file"),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderView {
    name: ResourceName,
    artifact: omnifs_core::ProviderId,
    catalog_name: String,
    version: Option<String>,
    description: Option<String>,
    phase: Option<ResourcePhase>,
    desired_revision: omnifs_core::ResourceRevision,
    observed_revision: Option<omnifs_core::ResourceRevision>,
    capabilities: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ProvidersResult {
    providers: Vec<ProviderView>,
}

impl ProviderArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            ProviderCommand::Add => add(output).await,
            ProviderCommand::Ls => list(output).await,
            ProviderCommand::Show { name } => show(name, output).await,
            ProviderCommand::Rm { name } => remove(name, output).await,
        }
    }
}

async fn add(output: Output) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let embedded = rpc.list_embedded_providers().await?;
    let source = crate::ui::prompt::Select::new("Provider source?")
        .items([SourceChoice::Embedded, SourceChoice::Local])
        .ask_with_output(&output)?;
    let provider_resolver = ProviderResolver::new(&rpc);
    let resolved_provider = match source {
        SourceChoice::Embedded => {
            let mut names = embedded
                .iter()
                .map(|metadata| metadata.reference.name.clone())
                .collect::<Vec<_>>();
            names.sort();
            let selector = crate::ui::prompt::Select::new("Which embedded provider?")
                .items(names)
                .ask_with_output(&output)?;
            provider_resolver.resolve(&selector, &embedded).await?
        },
        SourceChoice::Local => {
            let path = crate::ui::prompt::Text::new("Local Wasm path").ask_with_output(&output)?;
            provider_resolver
                .resolve_local(std::path::Path::new(&path))
                .await?
        },
    };
    crate::commands::mount::render_consent_block(&output, &resolved_provider.manifest);
    for line in crate::capability::consent_detail(&resolved_provider.manifest) {
        output.narrate(line);
    }
    output.narrate("Import retains content by digest. It grants no authority.");
    let name = crate::ui::prompt::Text::new("Provider resource name")
        .with_default(&resolved_provider.manifest.id)
        .ask_with_output(&output)?;
    let definition = ProviderDefinition {
        name: ResourceName::new(name)?,
        artifact: resolved_provider.reference.id,
    };
    let title = format!("Add provider `{}`", definition.name);
    let result = match resource_flow::edit_resources_and_wait(
        &rpc,
        &output,
        &title,
        move |resources| {
            resources.retain(|resource| resource.key() != definition.key());
            resources.push(ResourceDefinition::Provider(definition));
            Ok(())
        },
        Vec::new(),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return resource_flow::finish_resource_error(&output, error),
    };
    output.outro(format!(
        "Provider ready at desired revision {}.",
        result.receipt.revision
    ));
    if crate::ui::consent::resolve_confirm(
        output.prompt_mode(),
        "Configure a Mount for this Provider now?",
        true,
        false,
        &output,
    )? {
        return crate::commands::mount::MountArgs {
            command: crate::commands::mount::MountCommand::Add,
        }
        .run(output)
        .await;
    }
    Ok(ExitCode::Success)
}

async fn list(output: Output) -> anyhow::Result<ExitCode> {
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let providers = provider_views(&rpc).await?;
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, ProvidersResult { providers })?;
    } else {
        output.report(render_providers(&providers));
    }
    Ok(ExitCode::Success)
}

async fn show(name: ResourceName, output: Output) -> anyhow::Result<ExitCode> {
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let provider = provider_views(&rpc)
        .await?
        .into_iter()
        .find(|provider| provider.name == name)
        .ok_or_else(|| anyhow::anyhow!("no Provider resource named `{name}`"))?;
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, &provider)?;
    } else {
        output.report(render_provider(&provider));
    }
    Ok(ExitCode::Success)
}

async fn remove(name: ResourceName, output: Output) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    let mut references = Vec::new();
    for resource in &snapshot.resources {
        match resource {
            ResourceDefinition::Credential(value) if value.provider == name => {
                references.push(value.key());
            },
            ResourceDefinition::Mount(value) if value.provider == name => {
                references.push(value.key());
            },
            _ => {},
        }
    }
    anyhow::ensure!(
        references.is_empty(),
        "provider `{name}` is still referenced by {}; remove those resources first",
        references
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    anyhow::ensure!(
        snapshot.resources.iter().any(|resource| {
            resource.kind() == ResourceKind::Provider && resource.name() == &name
        }),
        "no Provider resource named `{name}`"
    );
    let title = format!("Remove provider `{name}`");
    let result = match resource_flow::edit_resources_and_wait(
        &rpc,
        &output,
        &title,
        move |resources| {
            resources.retain(|resource| {
                resource.kind() != ResourceKind::Provider || resource.name() != &name
            });
            Ok(())
        },
        Vec::new(),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return resource_flow::finish_resource_error(&output, error),
    };
    output.outro(format!(
        "Provider removed at desired revision {}. The retained artifact remains content-addressed.",
        result.receipt.revision
    ));
    Ok(ExitCode::Success)
}

async fn provider_views(rpc: &RpcClient) -> anyhow::Result<Vec<ProviderView>> {
    let snapshot = rpc.resources().await?;
    let mut providers = Vec::new();
    for resource in &snapshot.resources {
        let ResourceDefinition::Provider(definition) = resource else {
            continue;
        };
        let metadata = rpc.provider_metadata(definition.artifact).await?;
        let manifest = metadata
            .as_ref()
            .and_then(|metadata| ProviderManifest::from_bytes(&metadata.manifest).ok());
        let status = snapshot.resource_statuses.iter().find(|status| {
            status.key.kind == ResourceKind::Provider && status.key.name == definition.name
        });
        providers.push(ProviderView {
            name: definition.name.clone(),
            artifact: definition.artifact,
            catalog_name: metadata.as_ref().map_or_else(
                || "unknown".to_owned(),
                |value| value.reference.name.clone(),
            ),
            version: metadata.and_then(|value| value.reference.version),
            description: manifest
                .as_ref()
                .and_then(|value| value.description.clone()),
            phase: status.map(|status| status.phase),
            desired_revision: status.map_or(snapshot.revision, |status| status.desired_revision),
            observed_revision: status.and_then(|status| status.observed_revision),
            capabilities: manifest
                .as_ref()
                .map_or_else(Vec::new, crate::capability::consent_detail),
        });
    }
    providers.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(providers)
}

fn render_providers(providers: &[ProviderView]) -> String {
    if providers.is_empty() {
        return "No Provider resources desired.\n".to_owned();
    }
    let mut output = String::from("NAME\tDIGEST\tPHASE\tREVISION\n");
    for provider in providers {
        let digest = provider.artifact.to_string();
        writeln!(
            output,
            "{}\t{}\t{}\t{}/{}",
            provider.name,
            &digest[..12],
            provider.phase.map_or_else(
                || "pending".to_owned(),
                |phase| format!("{phase:?}").to_lowercase()
            ),
            provider.desired_revision,
            provider
                .observed_revision
                .map_or_else(|| "-".to_owned(), |revision| revision.to_string()),
        )
        .expect("writing to a String cannot fail");
    }
    output
}

fn render_provider(provider: &ProviderView) -> String {
    let mut output = render_providers(std::slice::from_ref(provider));
    if let Some(description) = &provider.description {
        writeln!(output, "\n{description}").expect("writing to a String cannot fail");
    }
    for line in &provider.capabilities {
        output.push_str(line);
        output.push('\n');
    }
    output
}
