//! Daemon-owned Attachment porcelain.
//!
//! An Attachment resource is desired OS exposure. Presence in `SQLite` asks the
//! daemon to start it; removing the resource asks the daemon to tear it down.
//! This module deliberately has no attach or detach operation.

use anyhow::{Context as _, Result, anyhow, ensure};
use clap::{Args, Subcommand};
use omnifs_api::{
    ActionReceipt, ApplyReceipt, AttachmentAccess, AttachmentDefinition, AttachmentStatus,
    GetAttachmentAccessRequest, ProgressSnapshot, ResourceDefinition, ResourcePhase,
    RestartAttachmentRequest,
};
use omnifs_bootstrap::Profile;
use omnifs_core::{
    ATTACHMENT_GUEST_LOCATION, ActionId, AttachmentProtocol, AttachmentRuntime, AttachmentSpec,
    ResourceKind, ResourceName, ResourceRevision,
};
use serde::Serialize;
use std::fmt::{self, Write as _};
use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::process::Command;

use crate::commands::daemon_start;
use crate::error::{ErrorVerdict, ExitCode, WithHint as _};
use crate::rpc::RpcClient;
use crate::ui::output::{Output, ResultVerdict};

#[derive(Args, Debug)]
pub struct AttachmentArgs {
    #[command(subcommand)]
    pub command: AttachmentCommand,
}

#[derive(Subcommand, Debug)]
pub enum AttachmentCommand {
    /// Add a platform-supported Attachment.
    Add(AddArgs),
    /// List desired Attachments and their observed state.
    Ls,
    /// Show one desired Attachment and its observed state.
    Show(NameArgs),
    /// Remove an Attachment from desired state.
    Rm(NameArgs),
    /// Restart an Attachment through a durable action.
    Restart(NameArgs),
    /// Enter the Attachment or run a command in its runtime.
    Shell(ShellArgs),
}

#[derive(Args, Debug, Clone, Default)]
pub struct AddArgs {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttachmentPair {
    protocol: AttachmentProtocol,
    runtime: AttachmentRuntime,
}

impl fmt::Display for AttachmentPair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} / {}", self.protocol, self.runtime)
    }
}

#[derive(Args, Debug, Clone)]
pub struct NameArgs {
    /// Attachment resource name.
    #[arg(value_name = "NAME")]
    pub name: String,
}

#[derive(Args, Debug, Clone)]
pub struct ShellArgs {
    /// Attachment resource name.
    #[arg(value_name = "NAME")]
    pub name: String,
    #[arg(
        num_args = 0..,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationResult {
    attachment: AttachmentDefinition,
    state: &'static str,
    revision: Option<ResourceRevision>,
    action_id: Option<ActionId>,
    receipt: Option<ApplyReceipt>,
    action_receipt: Option<ActionReceipt>,
    committed: bool,
    follow: Option<String>,
    snapshot: Option<ProgressSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResult {
    attachments: Vec<ListRow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListRow {
    name: ResourceName,
    protocol: AttachmentProtocol,
    runtime: AttachmentRuntime,
    location: PathBuf,
    phase: &'static str,
    desired_revision: ResourceRevision,
    observed_revision: Option<ResourceRevision>,
    detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShowResult {
    status: AttachmentStatus,
}

impl AttachmentArgs {
    pub async fn run(self, output: Output) -> Result<ExitCode> {
        match self.command {
            AttachmentCommand::Add(args) => add(args, output).await,
            AttachmentCommand::Ls => list(output).await,
            AttachmentCommand::Show(args) => show(args, output).await,
            AttachmentCommand::Rm(args) => remove(args, output).await,
            AttachmentCommand::Restart(args) => restart(args, output).await,
            AttachmentCommand::Shell(args) => shell(args, output).await,
        }
    }
}

async fn add(_args: AddArgs, output: Output) -> Result<ExitCode> {
    crate::commands::resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let pairs = available_pairs();
    ensure!(
        !pairs.is_empty(),
        "this platform has no supported Attachment runtime"
    );
    let pair = crate::ui::prompt::Select::new("Protocol and runtime?")
        .items(pairs)
        .ask_with_output(&output)?;
    let default_name = format!("{}-{}", pair.protocol, pair.runtime);
    let name = crate::ui::prompt::Text::new("Attachment name")
        .with_default(&default_name)
        .ask_with_output(&output)?;
    let name = ResourceName::new(name)?;
    let location = if pair.runtime == AttachmentRuntime::Host {
        let default = Profile::resolve()?
            .root()
            .join("attachments")
            .join(name.as_str());
        let value = crate::ui::prompt::Text::new("Host mount location")
            .with_default(default.to_string_lossy().into_owned())
            .ask_with_output(&output)?;
        Some(PathBuf::from(value))
    } else {
        None
    };
    let definition = definition_for_pair(name, pair, location)?;
    output.narrate("The Attachment remains attached while this resource is desired.");
    let rpc = RpcClient::resolve()?;
    let desired = definition.clone();
    let result = match crate::commands::resource_flow::edit_resources_and_wait(
        &rpc,
        &output,
        "Apply Attachment resource",
        move |resources| {
            resources.retain(|resource| resource.key() != desired.key());
            resources.push(ResourceDefinition::Attachment(desired));
            Ok(())
        },
        Vec::new(),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return crate::commands::resource_flow::finish_resource_error(&output, error);
        },
    };
    finish_result(
        &output,
        MutationResult {
            attachment: definition,
            state: "ready",
            revision: Some(result.receipt.revision),
            action_id: None,
            receipt: Some(result.receipt),
            action_receipt: None,
            committed: true,
            follow: Some(format!(
                "omnifs status --follow --revision {}",
                result.snapshot.desired_revision
            )),
            snapshot: Some(result.snapshot),
        },
    )
}

async fn list(output: Output) -> Result<ExitCode> {
    daemon_start::start(&output).await?;
    let snapshot = RpcClient::resolve()?.resources().await?;
    let mut attachments = Vec::new();
    for resource in snapshot.resources {
        let ResourceDefinition::Attachment(definition) = resource else {
            continue;
        };
        let status = snapshot.resource_statuses.iter().find(|status| {
            status.key.kind == ResourceKind::Attachment && status.key.name == definition.name
        });
        attachments.push(ListRow {
            name: definition.name.clone(),
            protocol: definition.spec.protocol(),
            runtime: definition.spec.runtime(),
            location: definition.spec.location().to_path_buf(),
            phase: status.map_or("pending", |status| resource_phase(status.phase)),
            desired_revision: status.map_or(snapshot.revision, |status| status.desired_revision),
            observed_revision: status.and_then(|status| status.observed_revision),
            detail: status.and_then(|status| status.detail.clone()),
        });
    }
    attachments.sort_by(|left, right| left.name.cmp(&right.name));
    let result = ListResult { attachments };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else if result.attachments.is_empty() {
        output.report("No Attachments desired.\n");
    } else {
        let mut rendered = String::from("NAME\tPROTOCOL\tRUNTIME\tPHASE\tLOCATION\n");
        for row in &result.attachments {
            writeln!(
                rendered,
                "{}\t{}\t{}\t{}\t{}",
                row.name,
                row.protocol,
                row.runtime,
                row.phase,
                row.location.display()
            )
            .expect("writing to a String cannot fail");
            if let Some(detail) = &row.detail {
                writeln!(rendered, "  {detail}").expect("writing to a String cannot fail");
            }
        }
        output.report(rendered);
    }
    Ok(ExitCode::Success)
}

async fn show(args: NameArgs, output: Output) -> Result<ExitCode> {
    daemon_start::start(&output).await?;
    let name = ResourceName::new(args.name)?;
    let status = RpcClient::resolve()?
        .attachment_status(name.clone())
        .await?
        .with_context(|| format!("Attachment {name} is not desired"))?;
    let result = ShowResult { status };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else {
        let status = &result.status;
        output.report(format!(
            "Attachment {}\n  protocol: {}\n  runtime: {}\n  location: {}\n  phase: {}\n  desired revision: {}\n  observed: {}\n",
            status.definition.name,
            status.definition.spec.protocol(),
            status.definition.spec.runtime(),
            status.definition.spec.location().display(),
            attachment_phase(status.phase),
            status.desired_revision,
            status.observed_version.is_some(),
        ));
        if let Some(detail) = &status.detail {
            output.report(format!("  detail: {detail}\n"));
        }
    }
    Ok(ExitCode::Success)
}

async fn remove(args: NameArgs, output: Output) -> Result<ExitCode> {
    crate::commands::resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let name = ResourceName::new(args.name)?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    let Some(definition) = snapshot
        .resources
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Attachment(value) if value.name == name => Some(value.clone()),
            _ => None,
        })
    else {
        return Err(anyhow!("Attachment {name} is not desired")).with_hint("omnifs attachment ls");
    };
    let removed_key = definition.key();
    let result = match crate::commands::resource_flow::edit_resources_and_wait(
        &rpc,
        &output,
        "Remove Attachment resource",
        move |resources| {
            resources.retain(|resource| resource.key() != removed_key);
            Ok(())
        },
        Vec::new(),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return crate::commands::resource_flow::finish_resource_error(&output, error);
        },
    };
    finish_result(
        &output,
        MutationResult {
            attachment: definition,
            state: "removed",
            revision: Some(result.receipt.revision),
            action_id: None,
            receipt: Some(result.receipt),
            action_receipt: None,
            committed: true,
            follow: Some(format!(
                "omnifs status --follow --revision {}",
                result.snapshot.desired_revision
            )),
            snapshot: Some(result.snapshot),
        },
    )
}

async fn restart(args: NameArgs, output: Output) -> Result<ExitCode> {
    crate::commands::resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let name = ResourceName::new(args.name)?;
    let rpc = RpcClient::resolve()?;
    let status = rpc
        .attachment_status(name.clone())
        .await?
        .with_context(|| format!("Attachment {name} is not desired"))?;
    let receipt = rpc
        .restart_attachment(&RestartAttachmentRequest {
            action_id: random_action_id()?,
            base_action_generation: status.action_generation,
            attachment: name,
        })
        .await?;
    let definition = status.definition.clone();
    match crate::commands::resource_flow::follow_progress(
        &rpc,
        omnifs_api::ProgressTarget::Action(receipt.action_id),
        &output,
    )
    .await
    .and_then(|progress| match progress {
        Some(crate::commands::resource_flow::FollowedProgress::Action(receipt)) => Ok(receipt),
        _ => Err(anyhow!(
            "Attachment action stream ended without a terminal receipt"
        )),
    }) {
        Ok(terminal_receipt) if terminal_receipt.phase == omnifs_api::ActionPhase::Ready => {
            finish_result(
                &output,
                MutationResult {
                    attachment: definition,
                    state: "ready",
                    revision: None,
                    action_id: Some(receipt.action_id),
                    receipt: None,
                    action_receipt: Some(terminal_receipt),
                    committed: true,
                    follow: Some(format!(
                        "omnifs status --follow --action {}",
                        receipt.action_id
                    )),
                    snapshot: None,
                },
            )
        },
        Ok(terminal_receipt) => {
            let error = anyhow!(
                "Attachment action {} failed{}{}",
                terminal_receipt.action_id,
                terminal_receipt
                    .error_code
                    .as_deref()
                    .map(|code| format!(" ({code})"))
                    .unwrap_or_default(),
                terminal_receipt
                    .detail
                    .as_deref()
                    .map(|detail| format!(": {detail}"))
                    .unwrap_or_default()
            );
            settle_action_error(&output, status.definition, &terminal_receipt, error)
        },
        Err(error) => settle_action_error(&output, status.definition, &receipt, error),
    }
}

async fn shell(args: ShellArgs, output: Output) -> Result<ExitCode> {
    output.require_human("attachment shell")?;
    daemon_start::start(&output).await?;
    let name = ResourceName::new(args.name)?;
    let interactive = std::io::stdin().is_terminal();
    let requested_command = args.command.clone();
    let access = RpcClient::resolve()?
        .attachment_access(&GetAttachmentAccessRequest {
            attachment: name.clone(),
            interactive,
            shell: None,
            command: requested_command.clone(),
        })
        .await?;
    match access {
        AttachmentAccess::HostPath(path) => {
            let mut command = if let Some(program) = requested_command.first() {
                let mut command = Command::new(program);
                command.args(&requested_command[1..]);
                command
            } else {
                Command::new("/bin/sh")
            };
            command.current_dir(path);
            let status = command.status().context("run Attachment command")?;
            ensure!(status.success(), "Attachment command exited with {status}");
        },
        AttachmentAccess::Command(invocation) => {
            let mut command = Command::new(invocation.program);
            command.args(invocation.args);
            if let Some(current_dir) = invocation.current_dir {
                command.current_dir(current_dir);
            }
            let status = command.status().context("run Attachment command")?;
            ensure!(status.success(), "Attachment command exited with {status}");
        },
    }
    Ok(ExitCode::Success)
}

fn settle_action_error(
    output: &Output,
    attachment: AttachmentDefinition,
    receipt: &ActionReceipt,
    error: anyhow::Error,
) -> Result<ExitCode> {
    let code = crate::error::exit_code(&error);
    let follow = format!("omnifs status --follow --action {}", receipt.action_id);
    if output.is_structured() {
        let result = MutationResult {
            attachment,
            state: "restart",
            revision: None,
            action_id: Some(receipt.action_id),
            receipt: None,
            action_receipt: Some(receipt.clone()),
            committed: true,
            follow: Some(follow.clone()),
            snapshot: None,
        };
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
                "Canceled. Attachment action {} continues. Follow with {follow}.",
                receipt.action_id
            ));
        }
        Err(error).with_hint(follow)
    }
}

fn finish_result(output: &Output, result: MutationResult) -> Result<ExitCode> {
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else {
        output.report(format!(
            "Attachment {} {} at {}\n",
            result.attachment.name,
            result.state,
            result
                .revision
                .map_or_else(|| "action".to_owned(), |revision| revision.to_string())
        ));
    }
    Ok(ExitCode::Success)
}

fn definition_for_pair(
    name: ResourceName,
    pair: AttachmentPair,
    location: Option<PathBuf>,
) -> Result<AttachmentDefinition> {
    let AttachmentPair { protocol, runtime } = pair;
    ensure!(supports(protocol, runtime), "{pair} is not supported");
    let profile_root = Profile::resolve()?.root().to_path_buf();
    let location = match runtime {
        AttachmentRuntime::Host => {
            location.unwrap_or_else(|| profile_root.join("attachments").join(name.as_str()))
        },
        AttachmentRuntime::Docker | AttachmentRuntime::Libkrun => {
            ensure!(
                location.is_none(),
                "guest Attachment runtimes own their location"
            );
            PathBuf::from(ATTACHMENT_GUEST_LOCATION)
        },
    };
    let docker_image = (runtime == AttachmentRuntime::Docker)
        .then(|| omnifs_fs_runtime::resolve_filesystem_image(None, None))
        .transpose()?
        .map(|value| value.to_string());
    let libkrun_guest_image = (runtime == AttachmentRuntime::Libkrun)
        .then(|| omnifs_fs_runtime::resolve_guest_image_reference(None));
    Ok(AttachmentDefinition {
        name,
        spec: AttachmentSpec::new(
            protocol,
            runtime,
            location,
            docker_image,
            libkrun_guest_image,
        )?,
    })
}

pub(crate) fn platform_default() -> Option<(AttachmentProtocol, AttachmentRuntime)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", _) => Some((AttachmentProtocol::Fuse, AttachmentRuntime::Host)),
        ("macos", "aarch64") => Some((AttachmentProtocol::Fuse, AttachmentRuntime::Libkrun)),
        ("macos", _) => Some((AttachmentProtocol::Nfs, AttachmentRuntime::Host)),
        _ => None,
    }
}

pub(crate) fn supports(protocol: AttachmentProtocol, runtime: AttachmentRuntime) -> bool {
    matches!(
        (
            std::env::consts::OS,
            std::env::consts::ARCH,
            protocol,
            runtime
        ),
        (
            "linux",
            _,
            AttachmentProtocol::Fuse,
            AttachmentRuntime::Host | AttachmentRuntime::Docker
        ) | (
            "linux" | "macos",
            _,
            AttachmentProtocol::Nfs,
            AttachmentRuntime::Host
        ) | (
            "macos",
            _,
            AttachmentProtocol::Fuse,
            AttachmentRuntime::Docker
        ) | (
            "macos",
            "aarch64",
            AttachmentProtocol::Fuse,
            AttachmentRuntime::Libkrun
        )
    )
}

fn available_pairs() -> Vec<AttachmentPair> {
    let recommended = platform_default();
    let mut pairs = [AttachmentProtocol::Fuse, AttachmentProtocol::Nfs]
        .into_iter()
        .flat_map(|protocol| {
            [
                AttachmentRuntime::Host,
                AttachmentRuntime::Docker,
                AttachmentRuntime::Libkrun,
            ]
            .into_iter()
            .filter(move |runtime| supports(protocol, *runtime))
            .map(move |runtime| AttachmentPair { protocol, runtime })
        })
        .collect::<Vec<_>>();
    pairs.sort_by_key(|pair| {
        (
            recommended != Some((pair.protocol, pair.runtime)),
            pair.protocol,
            pair.runtime,
        )
    });
    pairs
}

pub(crate) fn recommended_definition() -> Result<Option<AttachmentDefinition>> {
    let Some((protocol, runtime)) = platform_default() else {
        return Ok(None);
    };
    let name = ResourceName::new(format!("{protocol}-{runtime}"))?;
    Ok(Some(definition_for_pair(
        name,
        AttachmentPair { protocol, runtime },
        None,
    )?))
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

const fn attachment_phase(phase: omnifs_api::AttachmentPhase) -> &'static str {
    match phase {
        omnifs_api::AttachmentPhase::Pending => "pending",
        omnifs_api::AttachmentPhase::WaitingForNamespace => "waiting-for-namespace",
        omnifs_api::AttachmentPhase::Starting => "starting",
        omnifs_api::AttachmentPhase::Ready => "ready",
        omnifs_api::AttachmentPhase::Stopping => "stopping",
        omnifs_api::AttachmentPhase::Retrying => "retrying",
        omnifs_api::AttachmentPhase::Failed => "failed",
        omnifs_api::AttachmentPhase::Deleting => "deleting",
    }
}

fn random_action_id() -> Result<ActionId> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generate Attachment action id")?;
    Ok(ActionId::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: AttachmentCommand,
    }

    #[test]
    fn grammar_has_no_attach_or_detach_and_preserves_shell_argv() {
        assert!(TestCli::try_parse_from(["attachment", "attach", "demo"]).is_err());
        assert!(TestCli::try_parse_from(["attachment", "detach", "demo"]).is_err());
        let parsed = TestCli::try_parse_from([
            "attachment",
            "shell",
            "demo",
            "--",
            "sh",
            "-lc",
            "printf '%s' 'two words'",
        ])
        .unwrap();
        let AttachmentCommand::Shell(shell) = parsed.command else {
            panic!("expected shell");
        };
        assert_eq!(shell.command, ["sh", "-lc", "printf '%s' 'two words'"]);
    }

    #[test]
    fn platform_default_is_supported() {
        if let Some((protocol, runtime)) = platform_default() {
            assert!(supports(protocol, runtime));
        }
    }

    #[test]
    fn recommended_pair_is_first() {
        if let Some((protocol, runtime)) = platform_default() {
            let first = available_pairs().into_iter().next().unwrap();
            assert_eq!((first.protocol, first.runtime), (protocol, runtime));
        }
    }

    #[test]
    fn resource_phase_is_stable() {
        assert_eq!(resource_phase(ResourcePhase::Ready), "ready");
        assert_eq!(resource_phase(ResourcePhase::Deleting), "deleting");
    }
}
