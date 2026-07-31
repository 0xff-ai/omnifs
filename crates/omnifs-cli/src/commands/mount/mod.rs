//! Interactive `Mount` resource authoring and read-only inventory.
//!
//! KCL is the automation surface. Public mount mutations collect only values
//! that cannot be inferred, edit the complete typed desired set, and use the
//! daemon's resource planner and revision progress stream.

pub(crate) mod spec_creation;
mod token_validation;

use std::fmt::{self, Write as _};

use anyhow::{Context as _, anyhow};
use clap::{Args, Subcommand};
use omnifs_api::{
    CredentialClientOverrides, CredentialDefinition, CredentialMaterial, CredentialMaterialSidecar,
    CredentialSubmission, MountResourceDefinition, ProviderDefinition, ResourceDefinition,
    ResourceLimits, ResourcePhase, ResourceStatus, SecretBytes,
};
use omnifs_auth::AuthScheme;
use omnifs_core::{ProviderId, ResourceKind, ResourceName, ResourceRevision};
use omnifs_provider::ProviderManifest;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Serialize;

use crate::auth::Auth;
use crate::commands::{daemon_start, resource_flow};
use crate::error::ExitCode;
use crate::rpc::RpcClient;
use crate::ui::output::{Output, ResultVerdict};
use crate::ui::table::{
    Block, Cell, Column, Priority, Report, ResourceRow, ResourceTable, StateToken, WidthPolicy,
};

#[derive(Args, Debug, Clone)]
pub struct MountArgs {
    #[command(subcommand)]
    pub command: MountCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum MountCommand {
    /// Add a Mount resource through an interactive wizard.
    Add,
    /// List desired Mount resources and their observed phases.
    Ls,
    /// Show one desired Mount resource.
    Show { name: ResourceName },
    /// Update one Mount resource through an interactive wizard.
    Update { name: ResourceName },
    /// Collect fresh material for the Mount's declared Credential.
    Reauth { name: ResourceName },
    /// Revoke the Mount's declared Credential upstream.
    Revoke { name: ResourceName },
    /// Remove one Mount resource.
    Rm { name: ResourceName },
}

#[derive(Debug, Clone)]
struct AvailableProvider {
    definition: ProviderDefinition,
    manifest: ProviderManifest,
}

impl fmt::Display for AvailableProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.definition.name.as_str())
    }
}

#[derive(Debug, Clone)]
enum CredentialChoice {
    Existing(ResourceName),
    Create,
}

impl fmt::Display for CredentialChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Existing(name) => write!(formatter, "{name}"),
            Self::Create => formatter.write_str("create a new Credential"),
        }
    }
}

struct CredentialSelection {
    name: ResourceName,
    definition: Option<CredentialDefinition>,
    material: Option<CredentialMaterialSidecar>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MountView {
    name: ResourceName,
    provider: ResourceName,
    credential: Option<ResourceName>,
    config: serde_json::Value,
    limits: Option<ResourceLimits>,
    phase: Option<ResourcePhase>,
    desired_revision: ResourceRevision,
    observed_revision: Option<ResourceRevision>,
    error_code: Option<String>,
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
struct MountsResult {
    mounts: Vec<MountView>,
}

impl MountArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            MountCommand::Add => add(output).await,
            MountCommand::Ls => list(output).await,
            MountCommand::Show { name } => show(name, output).await,
            MountCommand::Update { name } => update(name, output).await,
            MountCommand::Reauth { name } => reauth(name, output).await,
            MountCommand::Revoke { name } => revoke(name, output).await,
            MountCommand::Rm { name } => remove(name, output).await,
        }
    }
}

async fn add(output: Output) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    let provider = select_provider(&rpc, &snapshot.resources, &output).await?;
    render_consent_block(&output, &provider.manifest);
    for line in crate::capability::consent_detail(&provider.manifest) {
        output.narrate(line);
    }

    let default_name = next_resource_name(
        &snapshot.resources,
        ResourceKind::Mount,
        &provider.manifest.default_mount,
    )?;
    let name = crate::ui::prompt::Text::new("Mount resource name")
        .with_default(default_name.as_str())
        .ask_with_output(&output)?;
    let name = ResourceName::new(name)?;
    anyhow::ensure!(
        !has_resource(&snapshot.resources, ResourceKind::Mount, &name),
        "Mount resource `{name}` already exists"
    );

    let config = spec_creation::create_config(&provider.manifest, &output, true)?
        .unwrap_or_else(empty_config);
    let credential = select_credential(&rpc, &snapshot.resources, &provider, &output, None).await?;
    let definition = MountResourceDefinition {
        name: name.clone(),
        provider: provider.definition.name.clone(),
        credential: credential.as_ref().map(|selected| selected.name.clone()),
        config,
        limits: manifest_limits(&provider.manifest),
    };
    let credential_definition = credential
        .as_ref()
        .and_then(|selected| selected.definition.clone());
    let material = credential
        .and_then(|selected| selected.material)
        .into_iter()
        .collect();
    let title = format!("Add Mount `{name}`");
    let result = match resource_flow::edit_resources_and_wait(
        &rpc,
        &output,
        &title,
        move |resources| {
            anyhow::ensure!(
                !has_resource(resources, ResourceKind::Mount, &definition.name),
                "Mount resource `{}` now exists; review a fresh plan",
                definition.name
            );
            if let Some(credential) = credential_definition {
                anyhow::ensure!(
                    !has_resource(resources, ResourceKind::Credential, &credential.name),
                    "Credential resource `{}` now exists; review a fresh plan",
                    credential.name
                );
                resources.push(ResourceDefinition::Credential(credential));
            }
            resources.push(ResourceDefinition::Mount(definition));
            Ok(())
        },
        material,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return resource_flow::finish_resource_error(&output, error),
    };
    output.outro(format!(
        "Mount `{name}` is ready at desired revision {}.",
        result.receipt.revision
    ));
    Ok(ExitCode::Success)
}

async fn update(name: ResourceName, output: Output) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    let current = snapshot
        .resources
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Mount(definition) if definition.name == name => {
                Some(definition.clone())
            },
            _ => None,
        })
        .ok_or_else(|| anyhow!("no Mount resource named `{name}`"))?;
    let provider_definition = snapshot
        .resources
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Provider(definition) if definition.name == current.provider => {
                Some(definition.clone())
            },
            _ => None,
        })
        .context("Mount resource references a missing Provider resource")?;
    let provider = load_provider(&rpc, provider_definition).await?;
    render_consent_block(&output, &provider.manifest);

    let config = spec_creation::update_config(&provider.manifest, &current.config, &output)?;
    let credential = select_credential(
        &rpc,
        &snapshot.resources,
        &provider,
        &output,
        current.credential.as_ref(),
    )
    .await?;
    let definition = MountResourceDefinition {
        name: name.clone(),
        provider: current.provider,
        credential: credential.as_ref().map(|selected| selected.name.clone()),
        config,
        limits: current.limits,
    };
    let credential_definition = credential
        .as_ref()
        .and_then(|selected| selected.definition.clone());
    let material = credential
        .and_then(|selected| selected.material)
        .into_iter()
        .collect();
    let title = format!("Update Mount `{name}`");
    let result = match resource_flow::edit_resources_and_wait(
        &rpc,
        &output,
        &title,
        move |resources| {
            let position = resources
                .iter()
                .position(|resource| resource.key() == definition.key())
                .ok_or_else(|| {
                    anyhow!(
                        "Mount resource `{}` was removed; review a fresh plan",
                        definition.name
                    )
                })?;
            if let Some(credential) = credential_definition {
                anyhow::ensure!(
                    !has_resource(resources, ResourceKind::Credential, &credential.name),
                    "Credential resource `{}` now exists; review a fresh plan",
                    credential.name
                );
                resources.push(ResourceDefinition::Credential(credential));
            }
            resources[position] = ResourceDefinition::Mount(definition);
            Ok(())
        },
        material,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return resource_flow::finish_resource_error(&output, error),
    };
    output.outro(format!(
        "Mount `{name}` is ready at desired revision {}.",
        result.receipt.revision
    ));
    Ok(ExitCode::Success)
}

async fn reauth(name: ResourceName, output: Output) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    let credential = mount_credential(&name, &output).await?;
    crate::commands::credential::authenticate_named(credential, output).await
}

async fn revoke(name: ResourceName, output: Output) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    let credential = mount_credential(&name, &output).await?;
    crate::commands::credential::revoke_named(credential, output).await
}

async fn mount_credential(name: &ResourceName, output: &Output) -> anyhow::Result<ResourceName> {
    daemon_start::start(output).await?;
    RpcClient::resolve()?
        .resources()
        .await?
        .resources
        .into_iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Mount(definition) if &definition.name == name => Some(
                definition
                    .credential
                    .ok_or_else(|| anyhow!("Mount resource `{name}` has no declared Credential")),
            ),
            _ => None,
        })
        .ok_or_else(|| anyhow!("no Mount resource named `{name}`"))?
}

async fn remove(name: ResourceName, output: Output) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    anyhow::ensure!(
        has_resource(&snapshot.resources, ResourceKind::Mount, &name),
        "no Mount resource named `{name}`"
    );
    let title = format!("Remove Mount `{name}`");
    let removed_name = name.clone();
    let result = match resource_flow::edit_resources_and_wait(
        &rpc,
        &output,
        &title,
        move |resources| {
            let before = resources.len();
            resources.retain(|resource| {
                resource.kind() != ResourceKind::Mount || resource.name() != &removed_name
            });
            anyhow::ensure!(
                resources.len() + 1 == before,
                "Mount resource `{removed_name}` was removed; review a fresh plan"
            );
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
        "Mount `{name}` was removed at desired revision {}.",
        result.receipt.revision
    ));
    Ok(ExitCode::Success)
}

async fn list(output: Output) -> anyhow::Result<ExitCode> {
    daemon_start::start(&output).await?;
    let mounts = mount_views(RpcClient::resolve()?.resources().await?);
    let verdict = views_verdict(&mounts);
    if output.is_structured() {
        output.emit_result(verdict, MountsResult { mounts })?;
    } else {
        output.report(render_mounts(&mounts));
    }
    Ok(exit_for_verdict(verdict))
}

async fn show(name: ResourceName, output: Output) -> anyhow::Result<ExitCode> {
    daemon_start::start(&output).await?;
    let mount = mount_views(RpcClient::resolve()?.resources().await?)
        .into_iter()
        .find(|mount| mount.name == name)
        .ok_or_else(|| anyhow!("no Mount resource named `{name}`"))?;
    let verdict = views_verdict(std::slice::from_ref(&mount));
    if output.is_structured() {
        output.emit_result(verdict, &mount)?;
    } else {
        output.report(render_mount(&mount));
    }
    Ok(exit_for_verdict(verdict))
}

async fn select_provider(
    rpc: &RpcClient,
    resources: &[ResourceDefinition],
    output: &Output,
) -> anyhow::Result<AvailableProvider> {
    let mut providers = Vec::new();
    for definition in resources.iter().filter_map(|resource| match resource {
        ResourceDefinition::Provider(definition) => Some(definition.clone()),
        _ => None,
    }) {
        providers.push(load_provider(rpc, definition).await?);
    }
    providers.sort_by(|left, right| left.definition.name.cmp(&right.definition.name));
    anyhow::ensure!(
        !providers.is_empty(),
        "no Provider resources exist; run `omnifs provider add` first"
    );
    let options = providers.into_iter().map(|provider| {
        let label = format!(
            "{}  {}",
            provider.definition.name,
            crate::provider_catalog::provider_auth_label(&provider.manifest)
        );
        let detail =
            vec![crate::provider_catalog::provider_description(&provider.manifest).to_owned()];
        (provider, label, detail)
    });
    crate::ui::prompt::Select::new("Which Provider resource?")
        .detailed_options(options)
        .ask_with_output(output)
}

async fn load_provider(
    rpc: &RpcClient,
    definition: ProviderDefinition,
) -> anyhow::Result<AvailableProvider> {
    let metadata = rpc
        .provider_metadata(definition.artifact)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "Provider resource `{}` references unavailable artifact {}",
                definition.name,
                definition.artifact
            )
        })?;
    let manifest = ProviderManifest::from_bytes(&metadata.manifest)
        .with_context(|| format!("parse metadata for Provider `{}`", definition.name))?;
    Ok(AvailableProvider {
        definition,
        manifest,
    })
}

async fn select_credential(
    rpc: &RpcClient,
    resources: &[ResourceDefinition],
    provider: &AvailableProvider,
    output: &Output,
    current: Option<&ResourceName>,
) -> anyhow::Result<Option<CredentialSelection>> {
    let Some(auth_manifest) = provider.manifest.auth.as_ref() else {
        return Ok(None);
    };
    let mut existing = resources
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Credential(definition)
                if definition.provider == provider.definition.name =>
            {
                Some(definition.name.clone())
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    existing.sort_by_key(|name| (current != Some(name), name.clone()));
    let mut choices = existing
        .into_iter()
        .map(CredentialChoice::Existing)
        .collect::<Vec<_>>();
    choices.push(CredentialChoice::Create);
    let choice = crate::ui::prompt::Select::new("Credential for this Mount?")
        .items(choices)
        .ask_with_output(output)?;
    match choice {
        CredentialChoice::Existing(name) => Ok(Some(CredentialSelection {
            name,
            definition: None,
            material: None,
        })),
        CredentialChoice::Create => {
            create_credential(rpc, resources, provider, auth_manifest, output)
                .await
                .map(Some)
        },
    }
}

async fn create_credential(
    _rpc: &RpcClient,
    resources: &[ResourceDefinition],
    provider: &AvailableProvider,
    auth_manifest: &omnifs_provider::ProviderAuthManifest,
    output: &Output,
) -> anyhow::Result<CredentialSelection> {
    let scheme_keys = auth_manifest
        .schemes
        .iter()
        .filter_map(AuthScheme::key)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !scheme_keys.is_empty(),
        "Provider `{}` declares no usable credential scheme",
        provider.definition.name
    );
    let scheme = if scheme_keys.len() == 1 {
        scheme_keys[0].clone()
    } else {
        crate::ui::prompt::Select::new("Authentication scheme?")
            .items(scheme_keys)
            .ask_with_output(output)?
    };
    let account = crate::ui::prompt::Text::new("Credential account")
        .with_default("default")
        .ask_with_output(output)?;
    let default_name = next_resource_name(
        resources,
        ResourceKind::Credential,
        provider.definition.name.as_str(),
    )?;
    let name = crate::ui::prompt::Text::new("Credential resource name")
        .with_default(default_name.as_str())
        .ask_with_output(output)?;
    let name = ResourceName::new(name)?;
    anyhow::ensure!(
        !has_resource(resources, ResourceKind::Credential, &name),
        "Credential resource `{name}` already exists"
    );
    let auth_manifest = auth_manifest.wasm_auth_manifest();
    let auth = Auth::from_scheme(Some(&auth_manifest), &scheme, Some(account.clone()))?;
    let submission = collect_credential_submission(provider, &auth, &account, output).await?;
    anyhow::ensure!(
        submission.provider == provider.definition.artifact
            && submission.scheme == scheme
            && submission.account_label == account,
        "credential flow returned an identity different from the declared resource"
    );
    let CredentialSubmission {
        material,
        overrides,
        ..
    } = submission;
    Ok(CredentialSelection {
        name: name.clone(),
        definition: Some(CredentialDefinition {
            name: name.clone(),
            provider: provider.definition.name.clone(),
            scheme,
            account,
        }),
        material: Some(CredentialMaterialSidecar {
            credential: name,
            material,
            overrides,
        }),
    })
}

async fn collect_credential_submission(
    provider: &AvailableProvider,
    auth: &Auth,
    account: &str,
    output: &Output,
) -> anyhow::Result<CredentialSubmission> {
    if auth.is_oauth() {
        return crate::auth::login::login_for_submission(
            provider.definition.artifact,
            &provider.manifest,
            auth,
            account,
            crate::auth::LoginInteractivity {
                no_browser: false,
                no_input: false,
                scopes: None,
            },
            output,
            crate::auth::auth_receipt_key_width(),
        )
        .await;
    }
    let scheme = auth.static_token_scheme(&provider.manifest)?;
    let guidance = provider
        .manifest
        .auth
        .as_ref()
        .map(|auth| auth.guidance_for(&scheme.key))
        .unwrap_or_default();
    if let Some(url) = &scheme.creation_url {
        output.narrate(format!("Create a token at {url}"));
    }
    for step in guidance.setup_steps {
        output.narrate(step);
    }
    let token = crate::ui::prompt::Password::new("Token").ask_with_output(output)?;
    anyhow::ensure!(!token.is_empty(), "token must not be empty");
    run_static_token_init(
        provider.definition.artifact,
        &provider.manifest,
        auth,
        SecretString::from(token),
        output,
    )
    .await
}

pub(crate) async fn run_static_token_init(
    provider: ProviderId,
    manifest: &ProviderManifest,
    auth: &Auth,
    token: SecretString,
    output: &Output,
) -> anyhow::Result<CredentialSubmission> {
    let scheme = auth.static_token_scheme(manifest)?;
    if let Some(validation) = scheme.validation.as_ref() {
        token_validation::validate_static_token(
            validation,
            scheme.header_name.as_deref().unwrap_or("Authorization"),
            &scheme.value_prefix,
            token.expose_secret(),
            output,
        )
        .await?;
    }
    Ok(CredentialSubmission {
        provider,
        scheme: scheme.key.clone(),
        account_label: auth.account_or_default().to_owned(),
        material: CredentialMaterial::StaticToken {
            token: SecretBytes::new(token.expose_secret().as_bytes().to_vec()),
        },
        overrides: CredentialClientOverrides {
            client_id: None,
            client_secret: None,
            redirect_uri: None,
            scopes: None,
        },
    })
}

/// Show one provider's declared meaning and enforced host needs before the
/// resource plan. Importing or selecting a provider never grants authority.
pub(crate) fn render_consent_block(output: &Output, manifest: &ProviderManifest) {
    let description = manifest
        .description
        .as_deref()
        .unwrap_or(&manifest.display_name);
    output.narrate(description);
    if let Some(needs) = crate::capability::compact_needs(manifest) {
        output.narrate(crate::ui::style::dim(
            needs,
            crate::ui::style::Stream::Stderr,
        ));
    }
    if let Some(limits) = crate::capability::compact_limits(manifest) {
        output.narrate(crate::ui::style::dim(
            limits,
            crate::ui::style::Stream::Stderr,
        ));
    }
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

fn empty_config() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn next_resource_name(
    resources: &[ResourceDefinition],
    kind: ResourceKind,
    preferred: &str,
) -> anyhow::Result<ResourceName> {
    for suffix in 1_u32..1000 {
        let candidate = if suffix == 1 {
            preferred.to_owned()
        } else {
            format!("{preferred}-{suffix}")
        };
        let candidate = ResourceName::new(candidate)?;
        if !has_resource(resources, kind, &candidate) {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "could not find an available {kind} name derived from `{preferred}`"
    ))
}

fn has_resource(resources: &[ResourceDefinition], kind: ResourceKind, name: &ResourceName) -> bool {
    resources
        .iter()
        .any(|resource| resource.kind() == kind && resource.name() == name)
}

fn mount_views(snapshot: omnifs_api::ResourceSnapshot) -> Vec<MountView> {
    let mut mounts = snapshot
        .resources
        .into_iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Mount(definition) => {
                let status = snapshot
                    .resource_statuses
                    .iter()
                    .find(|status| status.key == definition.key());
                Some(mount_view(definition, status, snapshot.revision))
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    mounts.sort_by(|left, right| left.name.cmp(&right.name));
    mounts
}

fn mount_view(
    definition: MountResourceDefinition,
    status: Option<&ResourceStatus>,
    revision: ResourceRevision,
) -> MountView {
    MountView {
        name: definition.name,
        provider: definition.provider,
        credential: definition.credential,
        config: definition.config,
        limits: definition.limits,
        phase: status.map(|status| status.phase),
        desired_revision: status.map_or(revision, |status| status.desired_revision),
        observed_revision: status.and_then(|status| status.observed_revision),
        error_code: status.and_then(|status| status.error_code.clone()),
        detail: status.and_then(|status| status.detail.clone()),
    }
}

fn views_verdict(mounts: &[MountView]) -> ResultVerdict {
    if mounts.iter().any(|mount| {
        matches!(
            mount.phase,
            Some(ResourcePhase::Failed | ResourcePhase::Blocked)
        )
    }) {
        ResultVerdict::Degraded
    } else {
        ResultVerdict::Ok
    }
}

fn exit_for_verdict(verdict: ResultVerdict) -> ExitCode {
    match verdict {
        ResultVerdict::Ok => ExitCode::Success,
        ResultVerdict::Degraded => ExitCode::Degraded,
    }
}

fn render_mounts(mounts: &[MountView]) -> String {
    let summary = if mounts.is_empty() {
        "none desired".to_owned()
    } else {
        format!("{} desired", mounts.len())
    };
    let mut table = ResourceTable::new(
        "Mounts",
        summary,
        vec![
            Column::new("NAME", Priority::Identity, WidthPolicy::Auto),
            Column::new("PROVIDER", Priority::Identity, WidthPolicy::Auto),
            Column::new("CREDENTIAL", Priority::Secondary, WidthPolicy::Auto),
            Column::new("DESIRED", Priority::Secondary, WidthPolicy::Auto),
            Column::new("OBSERVED", Priority::Detail, WidthPolicy::Auto),
            Column::new("PHASE", Priority::Essential, WidthPolicy::Auto),
        ],
    );
    for mount in mounts {
        let state = phase_state(mount.phase);
        table.push(ResourceRow::new(
            [
                Cell::new(mount.name.to_string()),
                Cell::new(mount.provider.to_string()),
                Cell::new(
                    mount
                        .credential
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), ToString::to_string),
                ),
                Cell::new(mount.desired_revision.to_string()),
                Cell::new(
                    mount
                        .observed_revision
                        .map_or_else(|| "-".to_owned(), |revision| revision.to_string()),
                ),
                Cell::state(state.clone()),
            ],
            state,
        ));
    }
    let mut report = Report::new();
    report.push(Block::Resources(table));
    report.render()
}

fn render_mount(mount: &MountView) -> String {
    let config = serde_json::to_string_pretty(&mount.config)
        .unwrap_or_else(|_| "<invalid config>".to_owned());
    let limits = mount.limits.as_ref().map_or_else(
        || "provider defaults".to_owned(),
        |limits| {
            format!(
                "memory={} MB, fetch={} bytes",
                limits
                    .max_memory_mb
                    .map_or_else(|| "default".to_owned(), |value| value.to_string()),
                limits
                    .max_fetch_blob_bytes
                    .map_or_else(|| "default".to_owned(), |value| value.to_string())
            )
        },
    );
    let mut rendered = format!(
        "Mount {}\n  provider: {}\n  credential: {}\n  desired revision: {}\n  observed revision: {}\n  phase: {}\n  limits: {}\n  config:\n",
        mount.name,
        mount.provider,
        mount
            .credential
            .as_ref()
            .map_or_else(|| "none".to_owned(), ToString::to_string),
        mount.desired_revision,
        mount
            .observed_revision
            .map_or_else(|| "none".to_owned(), |revision| revision.to_string()),
        phase_name(mount.phase),
        limits,
    );
    for line in config.lines() {
        rendered.push_str("    ");
        rendered.push_str(line);
        rendered.push('\n');
    }
    if let Some(code) = &mount.error_code {
        writeln!(rendered, "  error: {code}").expect("writing to a String cannot fail");
    }
    if let Some(detail) = &mount.detail {
        writeln!(rendered, "  detail: {detail}").expect("writing to a String cannot fail");
    }
    rendered
}

fn phase_name(phase: Option<ResourcePhase>) -> String {
    phase.map_or_else(
        || "pending".to_owned(),
        |phase| format!("{phase:?}").to_ascii_lowercase(),
    )
}

fn phase_state(phase: Option<ResourcePhase>) -> StateToken {
    let label = phase_name(phase);
    match phase {
        Some(ResourcePhase::Ready) => StateToken::positive(label),
        Some(ResourcePhase::Failed) => StateToken::failure(label),
        Some(ResourcePhase::Blocked | ResourcePhase::Retrying) => StateToken::attention(label),
        _ => StateToken::neutral(label),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str) -> ResourceDefinition {
        ResourceDefinition::Provider(ProviderDefinition {
            name: ResourceName::new(name).unwrap(),
            artifact: ProviderId::from_digest([7; 32]),
        })
    }

    #[test]
    fn next_name_never_overwrites_an_existing_mount() {
        let resources = vec![
            provider("demo"),
            ResourceDefinition::Mount(MountResourceDefinition {
                name: ResourceName::new("demo").unwrap(),
                provider: ResourceName::new("demo").unwrap(),
                credential: None,
                config: empty_config(),
                limits: None,
            }),
        ];
        assert_eq!(
            next_resource_name(&resources, ResourceKind::Mount, "demo").unwrap(),
            ResourceName::new("demo-2").unwrap()
        );
    }

    #[test]
    fn failed_mounts_degrade_read_results() {
        let mount = MountView {
            name: ResourceName::new("demo").unwrap(),
            provider: ResourceName::new("provider").unwrap(),
            credential: None,
            config: empty_config(),
            limits: None,
            phase: Some(ResourcePhase::Failed),
            desired_revision: ResourceRevision::new(2),
            observed_revision: None,
            error_code: Some("compile-failed".to_owned()),
            detail: Some("provider failed".to_owned()),
        };
        assert_eq!(views_verdict(&[mount]), ResultVerdict::Degraded);
    }

    #[test]
    fn mount_table_uses_resource_phase_and_revisions() {
        let mount = MountView {
            name: ResourceName::new("demo").unwrap(),
            provider: ResourceName::new("provider").unwrap(),
            credential: None,
            config: empty_config(),
            limits: None,
            phase: Some(ResourcePhase::Preparing),
            desired_revision: ResourceRevision::new(4),
            observed_revision: Some(ResourceRevision::new(3)),
            error_code: None,
            detail: None,
        };
        let rendered = render_mounts(&[mount]);
        assert!(rendered.contains("demo"));
        assert!(rendered.contains("preparing"));
        assert!(rendered.contains('4'));
        assert!(rendered.contains('3'));
    }
}
