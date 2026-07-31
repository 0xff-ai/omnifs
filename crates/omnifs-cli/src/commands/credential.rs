//! Credential resource and durable action porcelain.
//!
//! Credential declarations are non-secret desired state. Secret material
//! crosses only the local control RPC in a typed action request, then this
//! command follows the action's durable progress receipt to a terminal state.

use anyhow::{Context as _, anyhow, ensure};
use clap::{Args, Subcommand};
use omnifs_api::{
    ActionReceipt, CredentialClientOverrides, CredentialDefinition, CredentialKind,
    CredentialMaterial, CredentialReceipt, CredentialStatus, CredentialStatusKind, ProgressTarget,
    ResourceDefinition, ResourcePhase, ResourceSnapshot, RevokeCredentialRequest, SecretBytes,
    SetCredentialMaterialRequest,
};
use omnifs_core::{ActionId, ResourceKind, ResourceName, ResourceRevision};
use omnifs_provider::ProviderManifest;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Serialize;

use crate::auth::{Auth, LoginInteractivity};
use crate::commands::{daemon_start, resource_flow};
use crate::error::{ErrorVerdict, ExitCode, WithHint as _};
use crate::rpc::RpcClient;
use crate::ui::consent::{Decision, Plan, Row};
use crate::ui::output::{Output, ResultVerdict};

#[derive(Args, Debug)]
pub struct CredentialArgs {
    #[command(subcommand)]
    pub command: CredentialCommand,
}

#[derive(Subcommand, Debug)]
pub enum CredentialCommand {
    /// Sign in to a declared OAuth credential.
    Login,
    /// Set static-token material from an environment variable.
    Set(SetArgs),
    /// List declared credentials without exposing their material.
    Ls,
    /// Show one declared credential without exposing its material.
    Show {
        /// Credential resource name.
        #[arg(value_name = "NAME")]
        name: ResourceName,
    },
    /// Remove an unused Credential resource and its local material.
    Rm {
        /// Credential resource name.
        #[arg(value_name = "NAME")]
        name: ResourceName,
    },
    /// Revoke a credential upstream and leave its declared slot empty.
    Revoke {
        /// Credential resource name.
        #[arg(value_name = "NAME")]
        name: ResourceName,
    },
}

#[derive(Args, Debug, Clone)]
pub struct SetArgs {
    /// Credential resource name.
    #[arg(value_name = "NAME")]
    pub name: ResourceName,
    /// Environment variable containing the token. The value is never printed.
    #[arg(long, value_name = "VARIABLE")]
    pub from_env: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CredentialView {
    name: ResourceName,
    provider: ResourceName,
    scheme: String,
    account: String,
    kind: Option<&'static str>,
    scopes: Vec<String>,
    phase: ResourcePhase,
    material_status: &'static str,
    action_generation: u64,
    desired_revision: ResourceRevision,
    observed_revision: Option<ResourceRevision>,
    error_code: Option<String>,
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
struct CredentialsResult {
    credentials: Vec<CredentialView>,
}

#[derive(Debug, Serialize)]
struct CredentialResult {
    credential: CredentialView,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CredentialActionResult {
    credential: CredentialDefinition,
    accepted: CredentialReceipt,
    terminal: Option<ActionReceipt>,
    follow: String,
}

struct CredentialContext {
    definition: CredentialDefinition,
    provider_artifact: omnifs_core::ProviderId,
    provider_catalog_name: String,
    manifest: ProviderManifest,
}

impl CredentialContext {
    fn key(&self) -> omnifs_api::CredentialKey {
        omnifs_api::CredentialKey {
            provider_name: self.provider_catalog_name.clone(),
            scheme: self.definition.scheme.clone(),
            account_label: self.definition.account.clone(),
        }
    }

    fn auth(&self) -> anyhow::Result<Auth> {
        let manifest = self
            .manifest
            .auth
            .as_ref()
            .map(omnifs_provider::ProviderAuthManifest::wasm_auth_manifest);
        Auth::from_scheme(
            manifest.as_ref(),
            &self.definition.scheme,
            Some(self.definition.account.clone()),
        )
    }
}

impl CredentialArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            CredentialCommand::Login => login(output).await,
            CredentialCommand::Set(args) => set(args, output).await,
            CredentialCommand::Ls => list(output).await,
            CredentialCommand::Show { name } => show(name, output).await,
            CredentialCommand::Rm { name } => remove(name, output).await,
            CredentialCommand::Revoke { name } => revoke_named(name, output).await,
        }
    }
}

async fn login(output: Output) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    let mut candidates = Vec::new();
    for definition in credential_definitions(&snapshot) {
        let context = resolve_context(&rpc, &snapshot, definition).await?;
        if context.auth().is_ok_and(|auth| auth.is_oauth()) {
            candidates.push(context.definition.name);
        }
    }
    candidates.sort();
    ensure!(
        !candidates.is_empty(),
        "no declared OAuth Credential resources are available"
    );
    let name = crate::ui::prompt::Select::new("Which credential?")
        .items(candidates)
        .ask_with_output(&output)?;
    authenticate_named_with_rpc(name, output, rpc, true).await
}

/// Collect fresh material for one exact declared credential and follow its
/// durable action. Mount re-authentication delegates here so OAuth and static
/// token actions share one secret boundary and generation precondition.
pub(crate) async fn authenticate_named(
    name: ResourceName,
    output: Output,
) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    authenticate_named_with_rpc(name, output, rpc, false).await
}

async fn authenticate_named_with_rpc(
    name: ResourceName,
    output: Output,
    rpc: RpcClient,
    oauth_only: bool,
) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    let context = load_context(&rpc, &name).await?;
    let auth = context.auth()?;
    ensure!(
        !oauth_only || auth.is_oauth(),
        "Credential `{name}` uses static-token scheme `{}`; use `credential set {name} --from-env VARIABLE`",
        context.definition.scheme
    );
    let submission = if auth.is_oauth() {
        crate::auth::login::login_for_submission(
            context.provider_artifact,
            &context.manifest,
            &auth,
            &context.definition.account,
            LoginInteractivity {
                no_browser: false,
                no_input: false,
                scopes: None,
            },
            &output,
            crate::auth::auth_receipt_key_width(),
        )
        .await?
    } else {
        let token = read_interactive_secret(&output)?;
        omnifs_api::CredentialSubmission {
            provider: context.provider_artifact,
            scheme: context.definition.scheme.clone(),
            account_label: context.definition.account.clone(),
            material: CredentialMaterial::StaticToken {
                token: SecretBytes::new(token.expose_secret().as_bytes().to_vec()),
            },
            overrides: empty_overrides(),
        }
    };
    ensure_submission_matches(&context, &submission)?;
    submit_material_action(
        &rpc,
        &output,
        context,
        submission.material,
        submission.overrides,
    )
    .await
}

fn read_interactive_secret(output: &Output) -> anyhow::Result<SecretString> {
    ensure!(
        crate::ui::prompt::is_terminal(),
        "no token source and stdin is not a terminal; use `credential set NAME --from-env VARIABLE`"
    );
    let value = crate::ui::prompt::Password::new("Token").ask_with_output(output)?;
    let value = value.trim();
    ensure!(!value.is_empty(), "token cannot be empty");
    Ok(SecretString::from(value.to_owned()))
}

async fn set(args: SetArgs, output: Output) -> anyhow::Result<ExitCode> {
    // This is the sole non-interactive secret mutation. Read the environment
    // boundary without formatting `VarError::NotUnicode`, whose payload would
    // contain the secret value.
    let token = read_env_secret(&args.from_env)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let context = load_context(&rpc, &args.name).await?;
    ensure!(
        !context.auth()?.is_oauth(),
        "Credential `{}` uses OAuth scheme `{}`; run `omnifs credential login` in a terminal",
        context.definition.name,
        context.definition.scheme
    );
    submit_material_action(
        &rpc,
        &output,
        context,
        CredentialMaterial::StaticToken {
            token: SecretBytes::new(token.expose_secret().as_bytes().to_vec()),
        },
        empty_overrides(),
    )
    .await
}

async fn submit_material_action(
    rpc: &RpcClient,
    output: &Output,
    context: CredentialContext,
    material: CredentialMaterial,
    overrides: CredentialClientOverrides,
) -> anyhow::Result<ExitCode> {
    let action_id = resource_flow::random_action_id()?;
    let follow = action_follow(action_id);
    output.narrate(format!(
        "Credential action `{action_id}` will continue in the daemon."
    ));
    let generation = credential_action_generation(rpc, &context).await?;
    let accepted = rpc
        .set_credential_material(&SetCredentialMaterialRequest {
            action_id,
            base_action_generation: generation,
            credential: context.definition.name.clone(),
            material,
            overrides,
        })
        .await
        .with_context(|| format!("submit credential action {action_id}"))
        .with_hint(follow.clone())?;
    settle_action(rpc, output, context.definition, accepted, follow).await
}

/// Revoke one exact declared credential and follow its durable action.
///
/// This remains interactive because revocation may have an upstream effect
/// and a failed or disconnected watch must not be reported as success.
pub(crate) async fn revoke_named(name: ResourceName, output: Output) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let context = load_context(&rpc, &name).await?;
    let mut plan = Plan::new(format!("Revoke credential `{name}` upstream"));
    plan.push(Row::remove(
        format!("credential/{name}"),
        "upstream access",
        format!(
            "{}/{}/{}",
            context.provider_catalog_name, context.definition.scheme, context.definition.account
        ),
    ));
    output.plan(&plan);
    output.narrate(
        "The Credential resource remains declared and needs new material after revocation.",
    );
    match Decision::resolve(
        output.prompt_mode(),
        false,
        "Revoke upstream access?",
        "--yes",
        &output,
    )? {
        Decision::Apply => {},
        Decision::DryRun => unreachable!("credential revoke has no dry-run mode"),
    }

    let action_id = resource_flow::random_action_id()?;
    let follow = action_follow(action_id);
    output.narrate(format!(
        "Credential action `{action_id}` will continue in the daemon."
    ));
    let generation = credential_action_generation(&rpc, &context).await?;
    let accepted = rpc
        .revoke_credential(&RevokeCredentialRequest {
            action_id,
            base_action_generation: generation,
            credential: context.definition.name.clone(),
        })
        .await
        .with_context(|| format!("submit credential revoke action {action_id}"))
        .with_hint(follow.clone())?;
    settle_action(&rpc, &output, context.definition, accepted, follow).await
}

async fn settle_action(
    rpc: &RpcClient,
    output: &Output,
    definition: CredentialDefinition,
    accepted: CredentialReceipt,
    follow: String,
) -> anyhow::Result<ExitCode> {
    let watched = resource_flow::follow_progress(
        rpc,
        ProgressTarget::Action(accepted.action.action_id),
        output,
    )
    .await
    .and_then(|progress| match progress {
        Some(resource_flow::FollowedProgress::Action(receipt)) => Ok(receipt),
        _ => Err(anyhow!(
            "credential action stream ended without a terminal receipt"
        )),
    });
    match watched {
        Ok(terminal) if terminal.phase == omnifs_api::ActionPhase::Ready => {
            let result = CredentialActionResult {
                credential: definition,
                accepted,
                terminal: Some(terminal),
                follow,
            };
            if output.is_structured() {
                output.emit_result(ResultVerdict::Ok, result)?;
            } else {
                output.report(format!(
                    "Credential {} ready (action {}, generation {})\n",
                    result.credential.name,
                    result.accepted.action.action_id,
                    result.accepted.action.action_generation
                ));
            }
            Ok(ExitCode::Success)
        },
        Ok(terminal) => {
            let error = anyhow!(
                "credential action {} failed{}{}",
                terminal.action_id,
                terminal
                    .error_code
                    .as_deref()
                    .map(|code| format!(" ({code})"))
                    .unwrap_or_default(),
                terminal
                    .detail
                    .as_deref()
                    .map(|detail| format!(": {detail}"))
                    .unwrap_or_default()
            );
            settle_action_error(output, definition, accepted, Some(terminal), follow, error)
        },
        Err(error) => settle_action_error(output, definition, accepted, None, follow, error),
    }
}

fn settle_action_error(
    output: &Output,
    definition: CredentialDefinition,
    accepted: CredentialReceipt,
    terminal: Option<ActionReceipt>,
    follow: String,
    error: anyhow::Error,
) -> anyhow::Result<ExitCode> {
    let code = crate::error::exit_code(&error);
    let result = CredentialActionResult {
        credential: definition,
        accepted,
        terminal,
        follow: follow.clone(),
    };
    if output.is_structured() {
        output.emit_detailed_error(
            if code == ExitCode::Canceled {
                ErrorVerdict::Canceled
            } else {
                ErrorVerdict::Failed
            },
            if code == ExitCode::Canceled {
                "canceled"
            } else {
                "action-failed"
            },
            code.code(),
            error.to_string(),
            follow,
            result,
        )?;
        Ok(code)
    } else {
        if code == ExitCode::Canceled {
            output.outro(format!(
                "Canceled. Credential action {} continues. Follow with {follow}.",
                result.accepted.action.action_id
            ));
        }
        Err(error).with_hint(follow)
    }
}

async fn list(output: Output) -> anyhow::Result<ExitCode> {
    let rpc = RpcClient::resolve()?;
    let credentials = credential_views(&rpc).await?;
    let result = CredentialsResult { credentials };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else {
        output.report(render_credentials(&result.credentials));
    }
    Ok(ExitCode::Success)
}

async fn show(name: ResourceName, output: Output) -> anyhow::Result<ExitCode> {
    let rpc = RpcClient::resolve()?;
    let credential = credential_views(&rpc)
        .await?
        .into_iter()
        .find(|credential| credential.name == name)
        .ok_or_else(|| anyhow!("no Credential resource named `{name}`"))
        .with_hint("omnifs credential ls")?;
    let result = CredentialResult { credential };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else {
        output.report(render_credentials(std::slice::from_ref(&result.credential)));
        if let Some(detail) = &result.credential.detail {
            output.report(format!("\n{detail}\n"));
        }
    }
    Ok(ExitCode::Success)
}

async fn remove(name: ResourceName, output: Output) -> anyhow::Result<ExitCode> {
    resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    ensure!(
        snapshot.resources.iter().any(|resource| {
            matches!(
                resource,
                ResourceDefinition::Credential(definition) if definition.name == name
            )
        }),
        "no Credential resource named `{name}`"
    );
    let references = mount_references(&snapshot, &name);
    ensure!(
        references.is_empty(),
        "Credential `{name}` is still referenced by Mount resources: {}; update or remove those Mounts first",
        references
            .iter()
            .map(ResourceName::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );
    output.narrate(
        "Removing the resource deletes local credential material after the active generation drains. It does not revoke upstream access.",
    );
    let result = match resource_flow::edit_resources_and_wait(
        &rpc,
        &output,
        &format!("Remove credential `{name}`"),
        move |resources| {
            resources.retain(|resource| {
                resource.kind() != ResourceKind::Credential || resource.name() != &name
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
        "Credential removed at desired revision {}. Upstream access was not revoked.",
        result.receipt.revision
    ));
    Ok(ExitCode::Success)
}

async fn load_context(rpc: &RpcClient, name: &ResourceName) -> anyhow::Result<CredentialContext> {
    let snapshot = rpc.resources().await?;
    let definition = credential_definitions(&snapshot)
        .into_iter()
        .find(|definition| definition.name == *name)
        .ok_or_else(|| anyhow!("no Credential resource named `{name}`"))
        .with_hint("omnifs credential ls")?;
    resolve_context(rpc, &snapshot, definition).await
}

async fn resolve_context(
    rpc: &RpcClient,
    snapshot: &ResourceSnapshot,
    definition: CredentialDefinition,
) -> anyhow::Result<CredentialContext> {
    let provider = snapshot
        .resources
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Provider(provider) if provider.name == definition.provider => {
                Some(provider)
            },
            _ => None,
        })
        .ok_or_else(|| {
            anyhow!(
                "Credential `{}` references missing Provider resource `{}`",
                definition.name,
                definition.provider
            )
        })?;
    let metadata = rpc
        .provider_metadata(provider.artifact)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "provider metadata is unavailable for Credential `{}`",
                definition.name
            )
        })?;
    ensure!(
        metadata.reference.id == provider.artifact,
        "provider metadata digest mismatch for Credential `{}`",
        definition.name
    );
    let manifest = ProviderManifest::from_bytes(&metadata.manifest)
        .context("parse daemon provider metadata")?;
    Ok(CredentialContext {
        definition,
        provider_artifact: provider.artifact,
        provider_catalog_name: metadata.reference.name,
        manifest,
    })
}

fn credential_definitions(snapshot: &ResourceSnapshot) -> Vec<CredentialDefinition> {
    snapshot
        .resources
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Credential(definition) => Some(definition.clone()),
            _ => None,
        })
        .collect()
}

async fn credential_action_generation(
    rpc: &RpcClient,
    context: &CredentialContext,
) -> anyhow::Result<u64> {
    Ok(rpc
        .credential_status(context.key())
        .await?
        .map_or(0, |status| status.action_generation))
}

async fn credential_views(rpc: &RpcClient) -> anyhow::Result<Vec<CredentialView>> {
    let snapshot = rpc.resources().await?;
    let mut credentials = Vec::new();
    for definition in credential_definitions(&snapshot) {
        let context = resolve_context(rpc, &snapshot, definition).await?;
        let stored = rpc.credential_status(context.key()).await?;
        let resource_status = snapshot.resource_statuses.iter().find(|status| {
            status.key.kind == ResourceKind::Credential
                && status.key.name == context.definition.name
        });
        credentials.push(credential_view(
            &snapshot,
            context.definition,
            resource_status,
            stored.as_ref(),
        ));
    }
    credentials.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(credentials)
}

fn credential_view(
    snapshot: &ResourceSnapshot,
    definition: CredentialDefinition,
    resource_status: Option<&omnifs_api::ResourceStatus>,
    stored: Option<&CredentialStatus>,
) -> CredentialView {
    CredentialView {
        name: definition.name,
        provider: definition.provider,
        scheme: definition.scheme,
        account: definition.account,
        kind: stored.map(|status| credential_kind(status.kind)),
        scopes: stored.map_or_else(Vec::new, |status| status.scopes.clone()),
        phase: resource_status.map_or(ResourcePhase::Pending, |status| status.phase),
        material_status: stored.map_or("needs_secret", |status| credential_status(status.status)),
        action_generation: stored.map_or(0, |status| status.action_generation),
        desired_revision: resource_status
            .map_or(snapshot.revision, |status| status.desired_revision),
        observed_revision: resource_status.and_then(|status| status.observed_revision),
        error_code: resource_status.and_then(|status| status.error_code.clone()),
        detail: resource_status.and_then(|status| status.detail.clone()),
    }
}

fn mount_references(snapshot: &ResourceSnapshot, credential: &ResourceName) -> Vec<ResourceName> {
    let mut references = snapshot
        .resources
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Mount(mount) if mount.credential.as_ref() == Some(credential) => {
                Some(mount.name.clone())
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    references.sort();
    references
}

fn ensure_submission_matches(
    context: &CredentialContext,
    submission: &omnifs_api::CredentialSubmission,
) -> anyhow::Result<()> {
    ensure!(
        submission.provider == context.provider_artifact
            && submission.scheme == context.definition.scheme
            && submission.account_label == context.definition.account,
        "collected credential material does not match Credential `{}`",
        context.definition.name
    );
    Ok(())
}

fn empty_overrides() -> CredentialClientOverrides {
    CredentialClientOverrides {
        client_id: None,
        client_secret: None,
        redirect_uri: None,
        scopes: None,
    }
}

fn read_env_secret(variable: &str) -> anyhow::Result<SecretString> {
    let value = std::env::var_os(variable)
        .ok_or_else(|| anyhow!("environment variable ${variable} is not set"))?;
    env_secret(variable, value)
}

fn env_secret(variable: &str, value: std::ffi::OsString) -> anyhow::Result<SecretString> {
    let value = value
        .into_string()
        .map_err(|_| anyhow!("environment variable ${variable} is not valid UTF-8"))?;
    let value = value.trim();
    ensure!(
        !value.is_empty(),
        "environment variable ${variable} is empty"
    );
    Ok(SecretString::from(value.to_owned()))
}

fn action_follow(action_id: ActionId) -> String {
    format!("omnifs status --follow --action {action_id}")
}

fn render_credentials(credentials: &[CredentialView]) -> String {
    use crate::ui::table::{
        Block, Cell, Column, Priority, Report, ResourceRow, ResourceTable, WidthPolicy,
    };

    if credentials.is_empty() {
        return "No Credential resources desired.\n".to_owned();
    }
    let mut table = ResourceTable::new(
        "Credentials",
        format!("{} resources", credentials.len()),
        vec![
            Column::new("Name", Priority::Identity, WidthPolicy::Auto),
            Column::new("Provider", Priority::Identity, WidthPolicy::Auto),
            Column::new("Scheme", Priority::Essential, WidthPolicy::Auto),
            Column::new("Account", Priority::Secondary, WidthPolicy::Auto),
            Column::new("Scopes", Priority::Detail, WidthPolicy::Auto),
            Column::new("Phase", Priority::Essential, WidthPolicy::Auto),
            Column::new("Material", Priority::Secondary, WidthPolicy::Auto),
            Column::new("Revision", Priority::Detail, WidthPolicy::Auto),
        ],
    );
    for credential in credentials {
        let state = resource_state(credential.phase);
        table.push(ResourceRow::new(
            [
                Cell::new(credential.name.to_string()),
                Cell::new(credential.provider.to_string()),
                Cell::new(&credential.scheme),
                Cell::new(&credential.account),
                Cell::new(if credential.scopes.is_empty() {
                    "none".to_owned()
                } else {
                    credential.scopes.join(", ")
                }),
                Cell::state(state.clone()),
                Cell::new(credential.material_status),
                Cell::new(format!(
                    "{}/{}",
                    credential.desired_revision,
                    credential
                        .observed_revision
                        .map_or_else(|| "-".to_owned(), |revision| revision.to_string())
                )),
            ],
            state,
        ));
    }
    let mut report = Report::new();
    report.push(Block::Resources(table));
    report.render()
}

const fn credential_kind(kind: CredentialKind) -> &'static str {
    match kind {
        CredentialKind::StaticToken => "static_token",
        CredentialKind::OAuth => "oauth",
    }
}

const fn credential_status(status: CredentialStatusKind) -> &'static str {
    match status {
        CredentialStatusKind::Active => "active",
        CredentialStatusKind::Blocked => "blocked",
        CredentialStatusKind::PendingRepublish => "pending_republish",
        CredentialStatusKind::RevocationPending => "revocation_pending",
        CredentialStatusKind::RevocationUnknown => "revocation_unknown",
        CredentialStatusKind::Deleted => "deleted",
    }
}

fn resource_state(phase: ResourcePhase) -> crate::ui::table::StateToken {
    use crate::ui::table::StateToken;

    match phase {
        ResourcePhase::Ready => StateToken::positive("ready"),
        ResourcePhase::Pending | ResourcePhase::Preparing => StateToken::attention("preparing"),
        ResourcePhase::Retrying => StateToken::attention("retrying"),
        ResourcePhase::Failed => StateToken::failure("failed"),
        ResourcePhase::Blocked => StateToken::failure("blocked"),
        ResourcePhase::Deleting => StateToken::neutral("deleting"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::{ResourceDefinition, ResourceStatus};
    use omnifs_core::{ResourceKey, ResourceRevision};

    fn name(value: &str) -> ResourceName {
        ResourceName::new(value).unwrap()
    }

    fn assert_no_secret_fields(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(fields) => {
                for (key, value) in fields {
                    assert!(
                        !matches!(
                            key.as_str(),
                            "material" | "token" | "accessToken" | "refreshToken" | "clientSecret"
                        ),
                        "secret field `{key}` reached action output"
                    );
                    assert_no_secret_fields(value);
                }
            },
            serde_json::Value::Array(values) => {
                for value in values {
                    assert_no_secret_fields(value);
                }
            },
            _ => {},
        }
    }

    #[test]
    fn mount_reference_scan_is_exact_and_sorted() {
        let snapshot = ResourceSnapshot {
            revision: ResourceRevision::new(3),
            desired_digest: omnifs_core::ResourceDigest::from_bytes([1; 32]),
            resources: vec![
                ResourceDefinition::Mount(omnifs_api::MountResourceDefinition {
                    name: name("zeta"),
                    provider: name("provider"),
                    credential: Some(name("account")),
                    config: serde_json::json!({}),
                    limits: None,
                }),
                ResourceDefinition::Mount(omnifs_api::MountResourceDefinition {
                    name: name("alpha"),
                    provider: name("provider"),
                    credential: Some(name("account")),
                    config: serde_json::json!({}),
                    limits: None,
                }),
                ResourceDefinition::Mount(omnifs_api::MountResourceDefinition {
                    name: name("other"),
                    provider: name("provider"),
                    credential: Some(name("other-account")),
                    config: serde_json::json!({}),
                    limits: None,
                }),
            ],
            resource_statuses: Vec::new(),
            serving_revision: None,
            providers: Vec::new(),
            serving: None,
        };
        assert_eq!(
            mount_references(&snapshot, &name("account")),
            vec![name("alpha"), name("zeta")]
        );
    }

    #[test]
    fn credential_view_contains_only_non_secret_status() {
        let snapshot = ResourceSnapshot {
            revision: ResourceRevision::new(4),
            desired_digest: omnifs_core::ResourceDigest::from_bytes([2; 32]),
            resources: Vec::new(),
            resource_statuses: Vec::new(),
            serving_revision: None,
            providers: Vec::new(),
            serving: None,
        };
        let definition = CredentialDefinition {
            name: name("github-default"),
            provider: name("github"),
            scheme: "token".into(),
            account: "default".into(),
        };
        let status = ResourceStatus {
            key: ResourceKey::new(ResourceKind::Credential, definition.name.clone()),
            desired_revision: snapshot.revision,
            observed_revision: None,
            phase: ResourcePhase::Blocked,
            error_code: Some("needs-secret".into()),
            detail: Some("credential material is required".into()),
        };
        let view = credential_view(&snapshot, definition, Some(&status), None);
        let encoded = serde_json::to_string(&view).unwrap();
        assert!(encoded.contains("needs_secret"));
        assert!(!encoded.contains("token-value"));
        assert!(!encoded.contains("SecretBytes"));
    }

    #[test]
    fn every_resource_phase_has_a_stable_human_token() {
        let phases = [
            (ResourcePhase::Pending, "▲ preparing"),
            (ResourcePhase::Preparing, "▲ preparing"),
            (ResourcePhase::Ready, "● ready"),
            (ResourcePhase::Retrying, "▲ retrying"),
            (ResourcePhase::Failed, "× failed"),
            (ResourcePhase::Blocked, "× blocked"),
            (ResourcePhase::Deleting, "○ deleting"),
        ];
        for (phase, expected) in phases {
            assert_eq!(resource_state(phase).render(false), expected);
        }
    }

    #[test]
    fn terminal_action_result_has_no_secret_field() {
        let action = ActionReceipt {
            action_id: ActionId::from_bytes([3; 16]),
            kind: omnifs_api::ActionKind::SetCredentialMaterial,
            target: ResourceKey::new(ResourceKind::Credential, name("account")),
            action_generation: 7,
            phase: omnifs_api::ActionPhase::Ready,
            error_code: None,
            detail: None,
        };
        let result = CredentialActionResult {
            credential: CredentialDefinition {
                name: name("account"),
                provider: name("provider"),
                scheme: "token".into(),
                account: "default".into(),
            },
            accepted: CredentialReceipt {
                action: action.clone(),
                status: CredentialStatusKind::Active,
            },
            terminal: Some(action),
            follow: "omnifs status --follow --action example".into(),
        };
        let encoded = serde_json::to_value(&result).unwrap();
        assert_no_secret_fields(&encoded);
        let text = encoded.to_string();
        assert!(!text.contains("token-value"));
        assert!(!text.contains("SecretBytes"));
    }

    #[cfg(unix)]
    #[test]
    fn invalid_environment_secret_error_names_only_the_variable() {
        use std::os::unix::ffi::OsStringExt as _;

        let secret = std::ffi::OsString::from_vec(vec![0xff, b's', b'e', b'c', b'r', b'e', b't']);
        let error = env_secret("OMNIFS_TEST_TOKEN", secret)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "environment variable $OMNIFS_TEST_TOKEN is not valid UTF-8"
        );
        assert!(!error.contains("secret"));
    }
}
