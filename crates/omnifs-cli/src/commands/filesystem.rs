//! Daemon-owned Filesystem porcelain.
//!
//! A Filesystem resource is desired OS exposure. Presence in `SQLite` asks the
//! daemon to start it; removing the resource asks the daemon to tear it down.
//! This module deliberately has no attach or detach operation.

use anyhow::{Context as _, Result, anyhow, ensure};
use clap::{Args, Subcommand};
use omnifs_api::{
    ActionReceipt, ApplyReceipt, DaemonInfo, FilesystemAccess, FilesystemDefinition,
    FilesystemStatus, GetFilesystemAccessRequest, ProgressSnapshot, ResourceDefinition,
    ResourcePhase, RestartFilesystemRequest,
};
use omnifs_bootstrap::Profile;
use omnifs_core::{
    FILESYSTEM_GUEST_LOCATION, FilesystemProtocol, FilesystemRuntime, FilesystemSpec, ResourceKind,
    ResourceName, ResourceRevision,
};
use serde::Serialize;
use std::fmt::{self, Write as _};
use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::process::Command;

use crate::commands::{daemon_start, resource_flow};
use crate::error::{ErrorVerdict, ExitCode, WithHint as _};
use crate::rpc::RpcClient;
use crate::ui::output::{Output, ResultVerdict};

#[derive(Args, Debug)]
pub struct FilesystemArgs {
    #[command(subcommand)]
    pub command: FilesystemCommand,
}

#[derive(Subcommand, Debug)]
pub enum FilesystemCommand {
    /// Add a platform-supported Filesystem.
    Add,
    /// List desired Filesystems and their observed state.
    Ls,
    /// Show one desired Filesystem and its observed state.
    Show {
        /// Filesystem resource name.
        #[arg(value_name = "NAME")]
        name: ResourceName,
    },
    /// Remove a Filesystem from desired state.
    Rm {
        /// Filesystem resource name.
        #[arg(value_name = "NAME")]
        name: ResourceName,
    },
    /// Restart a Filesystem through a durable action.
    Restart {
        /// Filesystem resource name.
        #[arg(value_name = "NAME")]
        name: ResourceName,
    },
    /// Enter the Filesystem or run a command in its runtime.
    Shell(ShellArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FilesystemPair {
    protocol: FilesystemProtocol,
    runtime: FilesystemRuntime,
}

impl fmt::Display for FilesystemPair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} / {}", self.protocol, self.runtime)
    }
}

#[derive(Args, Debug, Clone)]
pub struct ShellArgs {
    /// Filesystem resource name.
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
    filesystem: FilesystemDefinition,
    state: &'static str,
    receipt: Option<ApplyReceipt>,
    action_receipt: Option<ActionReceipt>,
    follow: String,
    snapshot: Option<ProgressSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResult {
    filesystems: Vec<ListRow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListRow {
    name: ResourceName,
    protocol: FilesystemProtocol,
    runtime: FilesystemRuntime,
    location: PathBuf,
    phase: &'static str,
    desired_revision: ResourceRevision,
    observed_revision: Option<ResourceRevision>,
    detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShowResult {
    status: FilesystemStatus,
}

impl FilesystemArgs {
    pub async fn run(self, output: Output) -> Result<ExitCode> {
        match self.command {
            FilesystemCommand::Add => add(output).await,
            FilesystemCommand::Ls => list(output).await,
            FilesystemCommand::Show { name } => show(name, output).await,
            FilesystemCommand::Rm { name } => remove(name, output).await,
            FilesystemCommand::Restart { name } => restart(name, output).await,
            FilesystemCommand::Shell(args) => shell(args, output).await,
        }
    }
}

async fn add(output: Output) -> Result<ExitCode> {
    crate::commands::resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let daemon_info = rpc.daemon_info().await?;
    let pairs = available_pairs(&daemon_info)?;
    ensure!(
        !pairs.is_empty(),
        "this platform has no supported Filesystem runtime"
    );
    let pair = crate::ui::prompt::Select::new("Protocol and runtime?")
        .items(pairs)
        .ask_with_output(&output)?;
    let default_name = format!("{}-{}", pair.protocol, pair.runtime);
    let name = crate::ui::prompt::Text::new("Filesystem name")
        .with_default(&default_name)
        .ask_with_output(&output)?;
    let name = ResourceName::new(name)?;
    let location = if pair.runtime == FilesystemRuntime::Host {
        let default = Profile::resolve()?
            .root()
            .join("filesystems")
            .join(name.as_str());
        let value = crate::ui::prompt::Text::new("Host mount location")
            .with_default(default.to_string_lossy().into_owned())
            .ask_with_output(&output)?;
        Some(PathBuf::from(value))
    } else {
        None
    };
    let definition = definition_for_pair(&daemon_info, name, pair, location)?;
    output.narrate("The Filesystem stays running while this resource is desired.");
    let desired = definition.clone();
    let result = match crate::commands::resource_flow::edit_resources_and_wait(
        &rpc,
        &output,
        "Apply Filesystem resource",
        move |resources| {
            resources.retain(|resource| resource.key() != desired.key());
            resources.push(ResourceDefinition::Filesystem(desired));
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
            filesystem: definition,
            state: "ready",
            receipt: Some(result.receipt),
            action_receipt: None,
            follow: format!(
                "omnifs status --follow --revision {}",
                result.snapshot.desired_revision
            ),
            snapshot: Some(result.snapshot),
        },
    )
}

async fn list(output: Output) -> Result<ExitCode> {
    daemon_start::start(&output).await?;
    let snapshot = RpcClient::resolve()?.resources().await?;
    let mut filesystems = Vec::new();
    for resource in snapshot.resources {
        let ResourceDefinition::Filesystem(definition) = resource else {
            continue;
        };
        let status = snapshot.resource_statuses.iter().find(|status| {
            status.key.kind == ResourceKind::Filesystem && status.key.name == definition.name
        });
        filesystems.push(ListRow {
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
    filesystems.sort_by(|left, right| left.name.cmp(&right.name));
    let result = ListResult { filesystems };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else if result.filesystems.is_empty() {
        output.report("No Filesystems desired.\n");
    } else {
        let mut rendered = String::from("NAME\tPROTOCOL\tRUNTIME\tPHASE\tLOCATION\n");
        for row in &result.filesystems {
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

async fn show(name: ResourceName, output: Output) -> Result<ExitCode> {
    daemon_start::start(&output).await?;
    let status = RpcClient::resolve()?
        .filesystem_status(name.clone())
        .await?
        .with_context(|| format!("Filesystem {name} is not desired"))?;
    let result = ShowResult { status };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else {
        let status = &result.status;
        output.report(format!(
            "Filesystem {}\n  protocol: {}\n  runtime: {}\n  location: {}\n  phase: {}\n  desired revision: {}\n  observed: {}\n",
            status.definition.name,
            status.definition.spec.protocol(),
            status.definition.spec.runtime(),
            status.definition.spec.location().display(),
            filesystem_phase(status.phase),
            status.desired_revision,
            status.observed_version.is_some(),
        ));
        if let Some(detail) = &status.detail {
            output.report(format!("  detail: {detail}\n"));
        }
    }
    Ok(ExitCode::Success)
}

async fn remove(name: ResourceName, output: Output) -> Result<ExitCode> {
    crate::commands::resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let snapshot = rpc.resources().await?;
    let Some(definition) = snapshot
        .resources
        .iter()
        .find_map(|resource| match resource {
            ResourceDefinition::Filesystem(value) if value.name == name => Some(value.clone()),
            _ => None,
        })
    else {
        return Err(anyhow!("Filesystem {name} is not desired")).with_hint("omnifs fs ls");
    };
    let removed_key = definition.key();
    let result = match crate::commands::resource_flow::edit_resources_and_wait(
        &rpc,
        &output,
        "Remove Filesystem resource",
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
            filesystem: definition,
            state: "removed",
            receipt: Some(result.receipt),
            action_receipt: None,
            follow: format!(
                "omnifs status --follow --revision {}",
                result.snapshot.desired_revision
            ),
            snapshot: Some(result.snapshot),
        },
    )
}

async fn restart(name: ResourceName, output: Output) -> Result<ExitCode> {
    crate::commands::resource_flow::ensure_interactive_mutation(&output)?;
    daemon_start::start(&output).await?;
    let rpc = RpcClient::resolve()?;
    let status = rpc
        .filesystem_status(name.clone())
        .await?
        .with_context(|| format!("Filesystem {name} is not desired"))?;
    let receipt = rpc
        .restart_filesystem(&RestartFilesystemRequest {
            action_id: resource_flow::random_action_id()?,
            base_action_generation: status.action_generation,
            filesystem: name,
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
            "Filesystem action stream ended without a terminal receipt"
        )),
    }) {
        Ok(terminal_receipt) if terminal_receipt.phase == omnifs_api::ActionPhase::Ready => {
            finish_result(
                &output,
                MutationResult {
                    filesystem: definition,
                    state: "ready",
                    receipt: None,
                    action_receipt: Some(terminal_receipt),
                    follow: format!("omnifs status --follow --action {}", receipt.action_id),
                    snapshot: None,
                },
            )
        },
        Ok(terminal_receipt) => {
            let error = anyhow!(
                "Filesystem action {} failed{}{}",
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
    output.require_human("fs shell")?;
    daemon_start::start(&output).await?;
    let name = ResourceName::new(args.name)?;
    let interactive = std::io::stdin().is_terminal();
    let requested_command = args.command.clone();
    let access = RpcClient::resolve()?
        .filesystem_access(&GetFilesystemAccessRequest {
            filesystem: name.clone(),
            interactive,
            shell: None,
            command: requested_command.clone(),
        })
        .await?;
    match access {
        FilesystemAccess::HostPath(path) => {
            let mut command = if let Some(program) = requested_command.first() {
                let mut command = Command::new(program);
                command.args(&requested_command[1..]);
                command
            } else {
                Command::new("/bin/sh")
            };
            command.current_dir(path);
            let status = command.status().context("run Filesystem command")?;
            ensure!(status.success(), "Filesystem command exited with {status}");
        },
        FilesystemAccess::Command(invocation) => {
            let mut command = Command::new(invocation.program);
            command.args(invocation.args);
            if let Some(current_dir) = invocation.current_dir {
                command.current_dir(current_dir);
            }
            let status = command.status().context("run Filesystem command")?;
            ensure!(status.success(), "Filesystem command exited with {status}");
        },
    }
    Ok(ExitCode::Success)
}

fn settle_action_error(
    output: &Output,
    filesystem: FilesystemDefinition,
    receipt: &ActionReceipt,
    error: anyhow::Error,
) -> Result<ExitCode> {
    let code = crate::error::exit_code(&error);
    let follow = format!("omnifs status --follow --action {}", receipt.action_id);
    if output.is_structured() {
        let result = MutationResult {
            filesystem,
            state: "restart",
            receipt: None,
            action_receipt: Some(receipt.clone()),
            follow: follow.clone(),
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
                "Canceled. Filesystem action {} continues. Follow with {follow}.",
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
        let target = result.receipt.as_ref().map_or_else(
            || "action".to_owned(),
            |receipt| receipt.revision.to_string(),
        );
        output.report(format!(
            "Filesystem {} {} at {}\n",
            result.filesystem.name, result.state, target
        ));
    }
    Ok(ExitCode::Success)
}

fn definition_for_pair(
    daemon_info: &DaemonInfo,
    name: ResourceName,
    pair: FilesystemPair,
    location: Option<PathBuf>,
) -> Result<FilesystemDefinition> {
    let FilesystemPair { protocol, runtime } = pair;
    ensure!(supports(daemon_info, pair), "{pair} is not supported");
    let profile_root = Profile::resolve()?.root().to_path_buf();
    let location = match runtime {
        FilesystemRuntime::Host => {
            location.unwrap_or_else(|| profile_root.join("filesystems").join(name.as_str()))
        },
        FilesystemRuntime::Docker | FilesystemRuntime::Libkrun => {
            ensure!(
                location.is_none(),
                "guest Filesystem runtimes own their location"
            );
            PathBuf::from(FILESYSTEM_GUEST_LOCATION)
        },
    };
    Ok(FilesystemDefinition {
        name,
        spec: FilesystemSpec::new(protocol, runtime, location, None, None)?,
    })
}

fn platform_default(daemon_info: &DaemonInfo) -> Result<Option<FilesystemPair>> {
    let default = daemon_info
        .platform_default_filesystem_pair
        .map(|(protocol, runtime)| FilesystemPair { protocol, runtime });
    if let Some(default) = default {
        ensure!(
            supports(daemon_info, default),
            "daemon advertised default Filesystem pair {default}, but it is not in supported_filesystem_pairs"
        );
    }
    Ok(default)
}

fn supports(daemon_info: &DaemonInfo, pair: FilesystemPair) -> bool {
    daemon_info
        .supported_filesystem_pairs
        .iter()
        .any(|&(protocol, runtime)| (protocol, runtime) == (pair.protocol, pair.runtime))
}

fn available_pairs(daemon_info: &DaemonInfo) -> Result<Vec<FilesystemPair>> {
    let recommended = platform_default(daemon_info)?;
    let mut pairs = daemon_info
        .supported_filesystem_pairs
        .iter()
        .map(|&(protocol, runtime)| FilesystemPair { protocol, runtime })
        .collect::<Vec<_>>();
    pairs.sort_by_key(|pair| (recommended != Some(*pair), pair.protocol, pair.runtime));
    Ok(pairs)
}

pub(crate) async fn recommended_definition(
    rpc: &RpcClient,
) -> Result<Option<FilesystemDefinition>> {
    let daemon_info = rpc.daemon_info().await?;
    let Some(pair) = platform_default(&daemon_info)? else {
        return Ok(None);
    };
    let name = ResourceName::new(format!("{}-{}", pair.protocol, pair.runtime))?;
    Ok(Some(definition_for_pair(&daemon_info, name, pair, None)?))
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

const fn filesystem_phase(phase: omnifs_api::FilesystemPhase) -> &'static str {
    match phase {
        omnifs_api::FilesystemPhase::Pending => "pending",
        omnifs_api::FilesystemPhase::WaitingForNamespace => "waiting-for-namespace",
        omnifs_api::FilesystemPhase::Starting => "starting",
        omnifs_api::FilesystemPhase::Ready => "ready",
        omnifs_api::FilesystemPhase::Stopping => "stopping",
        omnifs_api::FilesystemPhase::Retrying => "retrying",
        omnifs_api::FilesystemPhase::Failed => "failed",
        omnifs_api::FilesystemPhase::Deleting => "deleting",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: FilesystemCommand,
    }

    #[test]
    fn grammar_has_no_attach_or_detach_and_preserves_shell_argv() {
        assert!(TestCli::try_parse_from(["fs", "attach", "demo"]).is_err());
        assert!(TestCli::try_parse_from(["fs", "detach", "demo"]).is_err());
        let parsed = TestCli::try_parse_from([
            "fs",
            "shell",
            "demo",
            "--",
            "sh",
            "-lc",
            "printf '%s' 'two words'",
        ])
        .unwrap();
        let FilesystemCommand::Shell(shell) = parsed.command else {
            panic!("expected shell");
        };
        assert_eq!(shell.command, ["sh", "-lc", "printf '%s' 'two words'"]);
    }

    #[test]
    fn platform_default_is_supported() {
        let daemon_info = daemon_info(
            &[
                (FilesystemProtocol::Nfs, FilesystemRuntime::Host),
                (FilesystemProtocol::Fuse, FilesystemRuntime::Host),
            ],
            Some((FilesystemProtocol::Fuse, FilesystemRuntime::Host)),
        );
        let default = platform_default(&daemon_info).unwrap().unwrap();
        assert!(supports(&daemon_info, default));
    }

    #[test]
    fn recommended_pair_is_first() {
        let daemon_info = daemon_info(
            &[
                (FilesystemProtocol::Nfs, FilesystemRuntime::Host),
                (FilesystemProtocol::Fuse, FilesystemRuntime::Docker),
                (FilesystemProtocol::Fuse, FilesystemRuntime::Host),
            ],
            Some((FilesystemProtocol::Fuse, FilesystemRuntime::Host)),
        );
        let first = available_pairs(&daemon_info)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            (first.protocol, first.runtime),
            (FilesystemProtocol::Fuse, FilesystemRuntime::Host)
        );
    }

    #[test]
    fn advertised_default_must_be_supported() {
        let daemon_info = daemon_info(
            &[(FilesystemProtocol::Nfs, FilesystemRuntime::Host)],
            Some((FilesystemProtocol::Fuse, FilesystemRuntime::Host)),
        );
        let error = available_pairs(&daemon_info).unwrap_err();
        assert_eq!(
            error.to_string(),
            "daemon advertised default Filesystem pair fuse / host, but it is not in supported_filesystem_pairs"
        );
    }

    #[test]
    fn unsupported_pair_keeps_picker_error() {
        let daemon_info = daemon_info(&[(FilesystemProtocol::Nfs, FilesystemRuntime::Host)], None);
        let error = definition_for_pair(
            &daemon_info,
            ResourceName::new("test").unwrap(),
            FilesystemPair {
                protocol: FilesystemProtocol::Fuse,
                runtime: FilesystemRuntime::Host,
            },
            None,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "fuse / host is not supported");
    }

    #[test]
    fn cli_definitions_leave_runtime_assets_unset() {
        let daemon_info = daemon_info(
            &[
                (FilesystemProtocol::Fuse, FilesystemRuntime::Docker),
                (FilesystemProtocol::Fuse, FilesystemRuntime::Libkrun),
            ],
            None,
        );
        let docker = definition_for_pair(
            &daemon_info,
            ResourceName::new("docker").unwrap(),
            FilesystemPair {
                protocol: FilesystemProtocol::Fuse,
                runtime: FilesystemRuntime::Docker,
            },
            None,
        )
        .unwrap();
        assert_eq!(docker.spec.docker_image(), None);
        assert_eq!(docker.spec.libkrun_guest_image(), None);

        let libkrun = definition_for_pair(
            &daemon_info,
            ResourceName::new("libkrun").unwrap(),
            FilesystemPair {
                protocol: FilesystemProtocol::Fuse,
                runtime: FilesystemRuntime::Libkrun,
            },
            None,
        )
        .unwrap();
        assert_eq!(libkrun.spec.docker_image(), None);
        assert_eq!(libkrun.spec.libkrun_guest_image(), None);
    }

    fn daemon_info(
        supported: &[(FilesystemProtocol, FilesystemRuntime)],
        default: Option<(FilesystemProtocol, FilesystemRuntime)>,
    ) -> DaemonInfo {
        DaemonInfo {
            version: "test".to_owned(),
            pid: 1,
            instance_id: "test".to_owned(),
            executable: "/bin/omnifs".into(),
            attach_unix: None,
            attach_tcp: None,
            supported_filesystem_pairs: supported.to_vec(),
            platform_default_filesystem_pair: default,
        }
    }

    #[test]
    fn resource_phase_is_stable() {
        assert_eq!(resource_phase(ResourcePhase::Ready), "ready");
        assert_eq!(resource_phase(ResourcePhase::Deleting), "deleting");
    }
}
