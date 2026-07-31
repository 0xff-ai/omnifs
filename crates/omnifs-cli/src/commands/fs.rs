//! Transitional filesystem commands backed by daemon-owned Attachments.

use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, anyhow, ensure};
use clap::{Args, Subcommand};
use omnifs_api::{
    API_VERSION, ActionPhase, ApplyResourcesRequest, AttachmentAccess, AttachmentDefinition,
    AttachmentProgress, AttachmentProgressStage, GetAttachmentAccessRequest, ProgressEventKind,
    ProgressSnapshot, ProgressTarget, ResourceDeclarations, ResourceDefinition, ResourcePhase,
    RestartAttachmentRequest,
};
use omnifs_bootstrap::Profile;
use omnifs_core::{
    ActionId, AttachmentSpec, MutationId, ResourceKind, ResourceName, ResourceRevision, fs,
};
use serde::Serialize;

use crate::error::{ExitCode, WithExitCode as _, WithHint as _};
use crate::legacy_filesystems::LegacyFilesystems;
use crate::rpc::RpcClient;
use crate::ui::output::{Output, ResultVerdict};

#[derive(Args, Debug)]
pub struct FsArgs {
    #[command(subcommand)]
    pub command: FsCommand,
}

#[derive(Subcommand, Debug)]
pub enum FsCommand {
    /// Create or update a desired filesystem Attachment
    Create(CreateArgs),
    /// Remove a desired filesystem Attachment
    Rm(NameArgs),
    /// Ensure a desired filesystem Attachment is ready
    Attach(NameArgs),
    /// Remove a desired filesystem Attachment
    Detach(NameArgs),
    /// Restart a desired filesystem Attachment
    Restart(NameArgs),
    /// Enter or locate a ready filesystem Attachment
    Shell(ShellArgs),
    /// List desired Attachments and read-only legacy filesystem specs
    Ls,
}

#[derive(Args, Debug, Clone)]
#[command(
    after_help = "Examples:\n  omnifs fs create --name work\n  omnifs fs create --name native --protocol nfs --runtime host --location /mnt/omnifs"
)]
pub struct CreateArgs {
    #[arg(long)]
    pub name: fs::Id,
    #[arg(long)]
    pub protocol: Option<fs::Protocol>,
    #[arg(long)]
    pub runtime: Option<fs::Runtime>,
    #[arg(long)]
    pub location: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct NameArgs {
    #[arg(long)]
    pub name: fs::Id,
}

#[derive(Args, Debug, Clone)]
#[command(
    after_help = "Examples:\n  omnifs fs shell --name work\n  omnifs fs shell --name work -- ls -la"
)]
pub struct ShellArgs {
    #[arg(long)]
    pub name: fs::Id,
    /// Shell to launch in a guest filesystem.
    #[arg(long)]
    pub shell: Option<String>,
    /// Command and arguments to run in the projected tree.
    #[arg(
        num_args = 0..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        conflicts_with = "shell"
    )]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionResult {
    attachment: AttachmentDefinition,
    state: &'static str,
    revision: Option<ResourceRevision>,
    action_id: Option<ActionId>,
    committed: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ListResult {
    attachments: Vec<ListRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    legacy_issues: Vec<crate::legacy_filesystems::LegacyIssue>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListRow {
    name: String,
    protocol: fs::Protocol,
    runtime: fs::Runtime,
    location: PathBuf,
    state: String,
    legacy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl FsArgs {
    pub async fn run(self, output: Output) -> Result<ExitCode> {
        match self.command {
            FsCommand::Create(args) => args.run(output).await,
            FsCommand::Rm(args) => remove(args, output, "removed").await,
            FsCommand::Attach(args) => attach(args, output).await,
            FsCommand::Detach(args) => remove(args, output, "detached").await,
            FsCommand::Restart(args) => restart(args, output).await,
            FsCommand::Shell(args) => shell(args, output).await,
            FsCommand::Ls => list(output).await,
        }
    }
}

impl CreateArgs {
    async fn run(self, output: Output) -> Result<ExitCode> {
        let definition = resolve_definition(self)?;
        crate::commands::daemon_start::start(&output).await?;
        let rpc = RpcClient::resolve()?;
        let revision = upsert_attachment(&rpc, definition.clone()).await?;
        wait_for_revision(&rpc, revision, &output).await?;
        finish_action(&output, definition, "ready", Some(revision), None, true)
    }
}

fn resolve_definition(args: CreateArgs) -> Result<AttachmentDefinition> {
    let (protocol, runtime) = resolve_pair(args.protocol, args.runtime)?;
    ensure!(
        supports(protocol, runtime),
        "{protocol}/{runtime} is not supported on {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let name = ResourceName::new(args.name.as_str())?;
    let location = match runtime {
        fs::Runtime::Host => args.location.unwrap_or(default_host_location(&name)?),
        fs::Runtime::Docker | fs::Runtime::Libkrun => {
            ensure!(
                args.location.is_none(),
                "the {runtime} runtime owns its location; --location is not allowed"
            );
            PathBuf::from(fs::GUEST_LOCATION)
        },
    };
    let docker_image = if runtime == fs::Runtime::Docker {
        Some(omnifs_fs_runtime::resolve_filesystem_image(None, None)?.to_string())
    } else {
        None
    };
    let libkrun_guest_image = (runtime == fs::Runtime::Libkrun)
        .then(|| omnifs_fs_runtime::resolve_guest_image_reference(None));
    let spec = AttachmentSpec::new(
        protocol,
        runtime,
        location,
        docker_image,
        libkrun_guest_image,
    )?;
    Ok(AttachmentDefinition { name, spec })
}

fn default_host_location(name: &ResourceName) -> Result<PathBuf> {
    Ok(default_host_location_under(
        Profile::resolve()?.root(),
        name,
    ))
}

fn default_host_location_under(profile: &Path, name: &ResourceName) -> PathBuf {
    profile.join("attachments").join(name.as_str())
}

fn resolve_pair(
    protocol: Option<fs::Protocol>,
    runtime: Option<fs::Runtime>,
) -> Result<(fs::Protocol, fs::Runtime)> {
    match (protocol, runtime) {
        (Some(fs::Protocol::Nfs), None) => Ok((fs::Protocol::Nfs, fs::Runtime::Host)),
        (None, Some(fs::Runtime::Docker)) => Ok((fs::Protocol::Fuse, fs::Runtime::Docker)),
        (None, Some(fs::Runtime::Libkrun)) => Ok((fs::Protocol::Fuse, fs::Runtime::Libkrun)),
        (Some(fs::Protocol::Fuse), None) => default_fuse_pair()
            .with_context(|| "FUSE has no default runtime on this platform; provide --runtime"),
        (None, Some(fs::Runtime::Host)) => {
            let (protocol, _) = platform_default().with_context(
                || "host has no default protocol on this platform; provide --protocol",
            )?;
            Ok((protocol, fs::Runtime::Host))
        },
        (None, None) => platform_default()
            .with_context(|| "this platform has no default filesystem protocol/runtime pair"),
        (Some(protocol), Some(runtime)) => Ok((protocol, runtime)),
    }
}

fn platform_default() -> Option<(fs::Protocol, fs::Runtime)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", _) => Some((fs::Protocol::Fuse, fs::Runtime::Host)),
        ("macos", "aarch64") => Some((fs::Protocol::Fuse, fs::Runtime::Libkrun)),
        ("macos", _) => Some((fs::Protocol::Nfs, fs::Runtime::Host)),
        _ => None,
    }
}

fn default_fuse_pair() -> Option<(fs::Protocol, fs::Runtime)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", _) => Some((fs::Protocol::Fuse, fs::Runtime::Host)),
        ("macos", "aarch64") => Some((fs::Protocol::Fuse, fs::Runtime::Libkrun)),
        _ => None,
    }
}

pub(crate) fn supports(protocol: fs::Protocol, runtime: fs::Runtime) -> bool {
    matches!(
        (
            std::env::consts::OS,
            std::env::consts::ARCH,
            protocol,
            runtime,
        ),
        (
            "linux",
            _,
            fs::Protocol::Fuse,
            fs::Runtime::Host | fs::Runtime::Docker
        ) | ("linux" | "macos", _, fs::Protocol::Nfs, fs::Runtime::Host)
            | ("macos", _, fs::Protocol::Fuse, fs::Runtime::Docker)
            | ("macos", "aarch64", fs::Protocol::Fuse, fs::Runtime::Libkrun)
    )
}

pub(crate) fn available_filesystems() -> Vec<(fs::Protocol, fs::Runtime)> {
    [fs::Protocol::Fuse, fs::Protocol::Nfs]
        .into_iter()
        .flat_map(|protocol| {
            [fs::Runtime::Host, fs::Runtime::Docker, fs::Runtime::Libkrun]
                .into_iter()
                .filter(move |runtime| supports(protocol, *runtime))
                .map(move |runtime| (protocol, runtime))
        })
        .collect()
}

pub(crate) fn default_runtime(protocol: fs::Protocol) -> Option<fs::Runtime> {
    match protocol {
        fs::Protocol::Fuse => default_fuse_pair().map(|(_, runtime)| runtime),
        fs::Protocol::Nfs if supports(fs::Protocol::Nfs, fs::Runtime::Host) => {
            Some(fs::Runtime::Host)
        },
        fs::Protocol::Nfs => None,
    }
}

pub(crate) fn recommended_filesystems() -> Vec<(fs::Protocol, fs::Runtime)> {
    available_filesystems()
        .into_iter()
        .filter(|&(protocol, runtime)| default_runtime(protocol) == Some(runtime))
        .collect()
}

pub(crate) fn recommended_filesystem_id() -> Result<Option<fs::Id>> {
    recommended_filesystems()
        .into_iter()
        .next()
        .map(|(protocol, runtime)| fs::Id::new(format!("{protocol}-{runtime}")))
        .transpose()
        .map_err(Into::into)
}

fn default_location_for(runtime: fs::Runtime, name: &ResourceName) -> Result<PathBuf> {
    match runtime {
        fs::Runtime::Host => default_host_location(name),
        fs::Runtime::Docker | fs::Runtime::Libkrun => Ok(PathBuf::from(fs::GUEST_LOCATION)),
    }
}

pub(crate) fn preview_filesystem_location(
    protocol: fs::Protocol,
    runtime: fs::Runtime,
) -> Result<PathBuf> {
    let name = ResourceName::new(format!("{protocol}-{runtime}"))?;
    default_location_for(runtime, &name)
}

pub(crate) async fn ensure_setup_filesystem(
    protocol: fs::Protocol,
    runtime: fs::Runtime,
    output: Output,
) -> Result<()> {
    crate::commands::daemon_start::start(&output).await?;
    let name = ResourceName::new(format!("{protocol}-{runtime}"))?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    let existing = attachment_definition(&snapshot.resources, &name).cloned();
    let definition = if let Some(existing) = existing {
        ensure!(
            existing.spec.protocol() == protocol && existing.spec.runtime() == runtime,
            "desired attachment `{name}` does not match setup's {protocol}/{runtime} choice"
        );
        existing
    } else {
        let docker_image = if runtime == fs::Runtime::Docker {
            Some(omnifs_fs_runtime::resolve_filesystem_image(None, None)?.to_string())
        } else {
            None
        };
        let libkrun_guest_image = (runtime == fs::Runtime::Libkrun)
            .then(|| omnifs_fs_runtime::resolve_guest_image_reference(None));
        AttachmentDefinition {
            name: name.clone(),
            spec: AttachmentSpec::new(
                protocol,
                runtime,
                default_location_for(runtime, &name)?,
                docker_image,
                libkrun_guest_image,
            )?,
        }
    };
    let revision = upsert_attachment_from_snapshot(&rpc, snapshot.resources, definition).await?;
    wait_for_revision(&rpc, revision, &output).await
}

async fn attach(args: NameArgs, output: Output) -> Result<ExitCode> {
    crate::commands::daemon_start::start(&output).await?;
    let name = resource_name(&args.name)?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    let Some(definition) = attachment_definition(&snapshot.resources, &name).cloned() else {
        return missing_desired_attachment(&args.name);
    };
    wait_for_revision(&rpc, snapshot.revision, &output).await?;
    finish_action(
        &output,
        definition,
        "ready",
        Some(snapshot.revision),
        None,
        false,
    )
}

async fn remove(args: NameArgs, output: Output, state: &'static str) -> Result<ExitCode> {
    crate::commands::daemon_start::start(&output).await?;
    let name = resource_name(&args.name)?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    let Some(definition) = attachment_definition(&snapshot.resources, &name).cloned() else {
        return missing_desired_attachment(&args.name);
    };
    let resources = snapshot
        .resources
        .into_iter()
        .filter(|resource| resource.key() != definition.key())
        .collect();
    let revision = apply_complete_set(&rpc, resources).await?;
    wait_for_revision(&rpc, revision, &output).await?;
    finish_action(&output, definition, state, Some(revision), None, true)
}

async fn restart(args: NameArgs, output: Output) -> Result<ExitCode> {
    crate::commands::daemon_start::start(&output).await?;
    let name = resource_name(&args.name)?;
    let rpc = RpcClient::resolve()?;
    let status = rpc
        .attachment_status(name.clone())
        .await?
        .with_context(|| format!("attachment `{name}` is not desired"))?;
    let request = RestartAttachmentRequest {
        action_id: random_action_id()?,
        base_action_generation: status.action_generation,
        attachment: name,
    };
    let receipt = rpc.restart_attachment(&request).await?;
    wait_for_action(&rpc, receipt.action_id, &output).await?;
    finish_action(
        &output,
        status.definition,
        "ready",
        None,
        Some(receipt.action_id),
        true,
    )
}

fn resource_name(id: &fs::Id) -> Result<ResourceName> {
    ResourceName::new(id.as_str()).map_err(Into::into)
}

fn missing_desired_attachment<T>(id: &fs::Id) -> Result<T> {
    let legacy = legacy_filesystems()?
        .scan()?
        .specs
        .into_iter()
        .find(|spec| spec.id() == id);
    let result = Err(anyhow!("attachment `{id}` is not desired"));
    if let Some(spec) = legacy {
        result.with_hint(format!(
            "Legacy detached config exists at {}. Import it explicitly with `omnifs fs create --name {} --protocol {} --runtime {}{}`",
            spec.location().display(),
            spec.id(),
            spec.protocol(),
            spec.runtime(),
            if spec.runtime() == fs::Runtime::Host {
                format!(" --location {}", spec.location().display())
            } else {
                String::new()
            }
        ))
    } else {
        result.with_hint(format!("Create it with `omnifs fs create --name {id}`"))
    }
}

fn legacy_filesystems() -> Result<LegacyFilesystems> {
    Ok(LegacyFilesystems::under_profile(Profile::resolve()?.root()))
}

async fn upsert_attachment(
    rpc: &RpcClient,
    definition: AttachmentDefinition,
) -> Result<ResourceRevision> {
    let snapshot = rpc.resources().await?;
    upsert_attachment_from_snapshot(rpc, snapshot.resources, definition).await
}

async fn upsert_attachment_from_snapshot(
    rpc: &RpcClient,
    mut resources: Vec<ResourceDefinition>,
    definition: AttachmentDefinition,
) -> Result<ResourceRevision> {
    resources.retain(|resource| resource.key() != definition.key());
    resources.push(ResourceDefinition::Attachment(definition));
    apply_complete_set(rpc, resources).await
}

async fn apply_complete_set(
    rpc: &RpcClient,
    resources: Vec<ResourceDefinition>,
) -> Result<ResourceRevision> {
    let declarations = ResourceDeclarations {
        api_version: API_VERSION.to_owned(),
        resources,
    };
    let plan = rpc.plan_resources(&declarations).await?;
    if plan
        .changes
        .iter()
        .all(|change| change.action == omnifs_api::ResourceChangeAction::Unchanged)
    {
        return Ok(plan.base_revision);
    }
    let request = ApplyResourcesRequest {
        mutation_id: random_mutation_id()?,
        base_revision: plan.base_revision,
        expected_desired_digest: plan.desired_digest,
        declarations: ResourceDeclarations {
            api_version: API_VERSION.to_owned(),
            resources: plan.normalized,
        },
        credential_material: Vec::new(),
    };
    Ok(rpc.apply_resources(&request).await?.revision)
}

fn random_mutation_id() -> Result<MutationId> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generate resource mutation id")?;
    Ok(MutationId::from_bytes(bytes))
}

fn random_action_id() -> Result<ActionId> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generate attachment action id")?;
    Ok(ActionId::from_bytes(bytes))
}

fn attachment_definition<'a>(
    resources: &'a [ResourceDefinition],
    name: &ResourceName,
) -> Option<&'a AttachmentDefinition> {
    resources.iter().find_map(|resource| match resource {
        ResourceDefinition::Attachment(definition) if &definition.name == name => Some(definition),
        _ => None,
    })
}

async fn wait_for_revision(
    rpc: &RpcClient,
    revision: ResourceRevision,
    output: &Output,
) -> Result<()> {
    let mut watch = rpc
        .watch_progress(ProgressTarget::DesiredRevision(revision))
        .await?;
    loop {
        let event = tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for Ctrl-C while following resource revision")?;
                return Err(anyhow!(
                    "revision {} is committed and daemon work continues; follow it with `omnifs status --follow --revision {}`",
                    revision.get(),
                    revision.get()
                ))
                .with_exit_code(ExitCode::Canceled);
            }
            event = watch.next() => event?,
        };
        let Some(event) = event else {
            return Err(anyhow!(
                "progress stream closed before revision {} reached a terminal state; daemon work continues",
                revision.get()
            ))
            .with_hint(format!(
                "omnifs status --follow --revision {}",
                revision.get()
            ));
        };
        match event.event {
            ProgressEventKind::Snapshot(snapshot) | ProgressEventKind::Resync(snapshot) => {
                match revision_snapshot_outcome(&snapshot, revision)? {
                    Some(()) => return Ok(()),
                    None => render_snapshot_attachments(output, &snapshot),
                }
            },
            ProgressEventKind::AttachmentProgress(progress) => {
                render_attachment_progress(output, &progress);
            },
            ProgressEventKind::RevisionReady(ready) if ready == revision => return Ok(()),
            ProgressEventKind::RevisionFailed {
                revision: failed,
                error_code,
                detail,
            } if failed == revision => {
                anyhow::bail!(
                    "revision {} failed ({error_code}): {detail}",
                    revision.get()
                );
            },
            ProgressEventKind::RevisionSuperseded {
                revision: replaced,
                replaced_by,
            } if replaced == revision => {
                return Err(anyhow!(
                    "revision {} was superseded by revision {}; daemon work continues",
                    replaced.get(),
                    replaced_by.get()
                ))
                .with_hint(format!(
                    "omnifs status --follow --revision {}",
                    replaced_by.get()
                ));
            },
            _ => {},
        }
    }
}

fn revision_snapshot_outcome(
    snapshot: &ProgressSnapshot,
    revision: ResourceRevision,
) -> Result<Option<()>> {
    if snapshot.desired_revision > revision {
        return Err(anyhow!(
            "revision {} was superseded by revision {}",
            revision.get(),
            snapshot.desired_revision.get()
        ))
        .with_hint(format!(
            "omnifs status --follow --revision {}",
            snapshot.desired_revision.get()
        ));
    }
    if let Some(status) = snapshot.resources.iter().find(|status| {
        status.desired_revision == revision
            && matches!(status.phase, ResourcePhase::Failed | ResourcePhase::Blocked)
    }) {
        anyhow::bail!(
            "{} failed{}{}",
            status.key,
            status
                .error_code
                .as_deref()
                .map(|code| format!(" ({code})"))
                .unwrap_or_default(),
            status
                .detail
                .as_deref()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default()
        );
    }
    let ready = snapshot
        .observed_revision
        .is_some_and(|observed| observed >= revision)
        && snapshot
            .resources
            .iter()
            .filter(|status| status.desired_revision == revision)
            .all(|status| status.phase == ResourcePhase::Ready);
    Ok(ready.then_some(()))
}

async fn wait_for_action(rpc: &RpcClient, action_id: ActionId, output: &Output) -> Result<()> {
    let mut watch = rpc
        .watch_progress(ProgressTarget::Action(action_id))
        .await?;
    loop {
        let event = tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for Ctrl-C while following attachment action")?;
                return Err(anyhow!(
                    "action {action_id} is accepted and daemon work continues; follow it with `omnifs status --follow --action {action_id}`"
                ))
                .with_exit_code(ExitCode::Canceled);
            }
            event = watch.next() => event?,
        };
        let Some(event) = event else {
            return Err(anyhow!(
                "progress stream closed before action {action_id} reached a terminal state; daemon work continues"
            ))
            .with_hint(format!("omnifs status --follow --action {action_id}"));
        };
        match event.event {
            ProgressEventKind::Snapshot(snapshot) | ProgressEventKind::Resync(snapshot) => {
                if let Some(receipt) = snapshot
                    .actions
                    .iter()
                    .find(|receipt| receipt.action_id == action_id)
                {
                    match receipt.phase {
                        ActionPhase::Ready => return Ok(()),
                        ActionPhase::Failed => {
                            anyhow::bail!(
                                "attachment action {action_id} failed{}{}",
                                receipt
                                    .error_code
                                    .as_deref()
                                    .map(|code| format!(" ({code})"))
                                    .unwrap_or_default(),
                                receipt
                                    .detail
                                    .as_deref()
                                    .map(|detail| format!(": {detail}"))
                                    .unwrap_or_default()
                            );
                        },
                        ActionPhase::Accepted | ActionPhase::Running | ActionPhase::Retrying => {},
                    }
                }
                render_snapshot_attachments(output, &snapshot);
            },
            ProgressEventKind::AttachmentProgress(progress) => {
                render_attachment_progress(output, &progress);
            },
            ProgressEventKind::ActionCompleted(receipt) if receipt.action_id == action_id => {
                return Ok(());
            },
            ProgressEventKind::ActionFailed {
                receipt,
                error_code,
                detail,
            } if receipt.action_id == action_id => {
                anyhow::bail!("attachment action {action_id} failed ({error_code}): {detail}");
            },
            _ => {},
        }
    }
}

fn render_snapshot_attachments(output: &Output, snapshot: &ProgressSnapshot) {
    for progress in &snapshot.attachments {
        render_attachment_progress(output, progress);
    }
}

fn render_attachment_progress(output: &Output, progress: &AttachmentProgress) {
    let bytes = match progress.total_bytes {
        Some(total) => format!(" {}/{} bytes", progress.completed_bytes, total),
        None if progress.completed_bytes > 0 => format!(" {} bytes", progress.completed_bytes),
        None => String::new(),
    };
    let retry = if progress.retry_count > 0 {
        format!(" retry {}", progress.retry_count)
    } else {
        String::new()
    };
    output.narrate(format!(
        "{}: {} on {}{}{} ({} queued, {} active)",
        progress.key.name,
        attachment_stage(progress.stage),
        progress.runtime,
        bytes,
        retry,
        progress.queued_attachments,
        progress.active_attachments
    ));
}

const fn attachment_stage(stage: AttachmentProgressStage) -> &'static str {
    match stage {
        AttachmentProgressStage::Queued => "queued",
        AttachmentProgressStage::WaitingForNamespace => "waiting for namespace",
        AttachmentProgressStage::PullingImage => "pulling image",
        AttachmentProgressStage::Materializing => "materializing image",
        AttachmentProgressStage::Starting => "starting runtime",
        AttachmentProgressStage::Mounting => "mounting",
        AttachmentProgressStage::Stopping => "stopping runtime",
        AttachmentProgressStage::Retrying => "retrying",
        AttachmentProgressStage::Deleting => "deleting",
        AttachmentProgressStage::Ready => "ready",
        AttachmentProgressStage::Failed => "failed",
    }
}

async fn shell(args: ShellArgs, output: Output) -> Result<ExitCode> {
    output.require_human("fs shell")?;
    crate::commands::daemon_start::start(&output).await?;
    let name = resource_name(&args.name)?;
    let rpc = RpcClient::resolve()?;
    let access = rpc
        .attachment_access(&GetAttachmentAccessRequest {
            attachment: name.clone(),
            interactive: std::io::stdin().is_terminal(),
            shell: args.shell.clone(),
            command: args.command.clone(),
        })
        .await?;
    match access {
        AttachmentAccess::HostPath(path) => {
            if let Some(program) = args.command.first() {
                let mut command = Command::new(program);
                command.args(&args.command[1..]).current_dir(&path);
                propagate(command, format!("run command in attachment `{name}`"))
            } else if let Some(shell) = args.shell.as_deref() {
                let mut command = Command::new(shell);
                command.current_dir(&path);
                propagate(command, format!("open shell in attachment `{name}`"))
            } else {
                output.report(format!(
                    "Attachment `{name}` is available at {}.\n",
                    path.display()
                ));
                Ok(ExitCode::Success)
            }
        },
        AttachmentAccess::Command(invocation) => {
            let mut command = Command::new(invocation.program);
            command.args(invocation.args);
            if let Some(current_dir) = invocation.current_dir {
                command.current_dir(current_dir);
            }
            propagate(command, format!("open attachment `{name}`"))
        },
    }
}

fn propagate(mut command: Command, context: String) -> Result<ExitCode> {
    let status = command.status().with_context(|| context)?;
    ensure!(
        status.success(),
        "filesystem shell command exited with {status}"
    );
    Ok(ExitCode::Success)
}

async fn list(output: Output) -> Result<ExitCode> {
    crate::commands::daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    let mut rows = snapshot
        .resources
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Attachment(definition) => {
                let status = snapshot.resource_statuses.iter().find(|status| {
                    status.key.kind == ResourceKind::Attachment
                        && status.key.name == definition.name
                });
                Some(ListRow {
                    name: definition.name.to_string(),
                    protocol: definition.spec.protocol(),
                    runtime: definition.spec.runtime(),
                    location: definition.spec.location().to_path_buf(),
                    state: status.map_or_else(
                        || "pending".to_owned(),
                        |status| resource_phase(status.phase).to_owned(),
                    ),
                    legacy: false,
                    detail: status.and_then(|status| status.detail.clone()),
                })
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    let legacy = legacy_filesystems()?.scan()?;
    for spec in legacy.specs {
        if rows.iter().any(|row| row.name == spec.id().as_str()) {
            continue;
        }
        rows.push(ListRow {
            name: spec.id().to_string(),
            protocol: spec.protocol(),
            runtime: spec.runtime(),
            location: spec.location().to_path_buf(),
            state: "legacy detached config".to_owned(),
            legacy: true,
            detail: Some(format!(
                "Import explicitly with `omnifs fs create --name {} --protocol {} --runtime {}`",
                spec.id(),
                spec.protocol(),
                spec.runtime()
            )),
        });
    }
    rows.sort_by(|left, right| left.name.cmp(&right.name));
    let result = ListResult {
        attachments: rows,
        legacy_issues: legacy.issues,
    };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, &result)?;
    } else if result.attachments.is_empty() {
        output.report("No attachments desired and no legacy filesystem specs found.\n");
    } else {
        let mut rendered = String::from("NAME\tPROTOCOL\tRUNTIME\tSTATE\tLOCATION\n");
        for row in &result.attachments {
            let _ = writeln!(
                rendered,
                "{}\t{}\t{}\t{}\t{}",
                row.name,
                row.protocol,
                row.runtime,
                row.state,
                row.location.display()
            );
            if let Some(detail) = &row.detail {
                let _ = writeln!(rendered, "  {detail}");
            }
        }
        for issue in &result.legacy_issues {
            let _ = writeln!(
                rendered,
                "legacy spec issue\t{}\t{}",
                issue.path.display(),
                issue.message
            );
        }
        output.report(rendered);
    }
    Ok(ExitCode::Success)
}

const fn resource_phase(phase: ResourcePhase) -> &'static str {
    match phase {
        ResourcePhase::Pending => "pending",
        ResourcePhase::Preparing => "preparing",
        ResourcePhase::Ready => "ready",
        ResourcePhase::Retrying => "retrying",
        ResourcePhase::Failed => "failed",
        ResourcePhase::Blocked => "blocked",
        ResourcePhase::Deleting => "deleting",
    }
}

fn finish_action(
    output: &Output,
    attachment: AttachmentDefinition,
    state: &'static str,
    revision: Option<ResourceRevision>,
    action_id: Option<ActionId>,
    committed: bool,
) -> Result<ExitCode> {
    let result = ActionResult {
        attachment,
        state,
        revision,
        action_id,
        committed,
    };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, &result)?;
    } else {
        let rows = [
            crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                "attachment",
                result.attachment.name.to_string(),
            ),
            crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                "protocol",
                result.attachment.spec.protocol().to_string(),
            ),
            crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                "runtime",
                result.attachment.spec.runtime().to_string(),
            ),
            crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                "location",
                result.attachment.spec.location().display().to_string(),
            ),
            crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Done,
                "state",
                state.to_owned(),
            ),
        ];
        let width = crate::ui::render::ledger_key_width(&rows);
        for row in &rows {
            output.ledger_row(row, width);
        }
        match state {
            "ready" if result.attachment.spec.runtime() == fs::Runtime::Host => {
                output.outro(format!(
                    "Files are at {}.",
                    result.attachment.spec.location().display()
                ));
            },
            "ready" => output.outro(format!(
                "Enter it: `omnifs fs shell --name {}`",
                result.attachment.name
            )),
            "detached" | "removed" => output.outro(format!(
                "Attachment `{}` is no longer desired.",
                result.attachment.name
            )),
            _ => {},
        }
    }
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_platform_matrix() {
        let expected = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", _) => Some((fs::Protocol::Fuse, fs::Runtime::Host)),
            ("macos", "aarch64") => Some((fs::Protocol::Fuse, fs::Runtime::Libkrun)),
            ("macos", _) => Some((fs::Protocol::Nfs, fs::Runtime::Host)),
            _ => None,
        };
        assert_eq!(platform_default(), expected);
    }

    #[test]
    fn host_default_is_profile_owned_without_client_filesystem_state() {
        let profile = Path::new("/tmp/omnifs-profile");
        let name = ResourceName::new("local").unwrap();
        assert_eq!(
            default_host_location_under(profile, &name),
            profile.join("attachments/local")
        );
    }

    #[test]
    fn legacy_specs_only_produce_an_import_hint() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path();
        let spec = fs::Spec::new(
            fs::Id::new("old").unwrap(),
            fs::Protocol::Nfs,
            fs::Runtime::Host,
            dir.path().join("mount"),
        )
        .unwrap();
        let specs = profile.join("client/filesystems/specs");
        std::fs::create_dir_all(&specs).unwrap();
        std::fs::write(specs.join("old.json"), serde_json::to_vec(&spec).unwrap()).unwrap();
        let listed = LegacyFilesystems::under_profile(profile).scan().unwrap();
        assert_eq!(listed.specs, vec![spec]);
        assert!(listed.issues.is_empty());
        assert!(
            profile
                .join("client/filesystems")
                .join("runtime")
                .read_dir()
                .is_err(),
            "a legacy read must not launch or create runtime state"
        );
    }
}
