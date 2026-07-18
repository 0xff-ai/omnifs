//! `omnifs frontend shell`: enter one observed guest frontend.

use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use clap::Args;

use crate::commands::frontend::{FrontendFilesystem, FrontendRuntime};
use crate::docker::{ContainerName, DockerClient, DockerRunner, DockerTarget};
use crate::frontend_container::{FRONTEND_DEV_IMAGE, frontend_container_name};
use crate::inventory::{FrontendState, Inventory};
use crate::libkrun_runner;
use crate::ui::output::Output;
use omnifs_workspace::Workspace;

#[derive(Args, Debug, Clone)]
pub struct ShellArgs {
    /// Filesystem exposed by the frontend.
    #[arg(value_enum)]
    pub filesystem: Option<FrontendFilesystem>,
    /// Guest runtime hosting the frontend.
    #[arg(long, value_enum)]
    pub runtime: Option<FrontendRuntime>,
    /// Shell to launch (defaults to the guest's `/bin/sh`).
    #[arg(long)]
    pub shell: Option<String>,
    /// Run a command in the projected tree instead of an interactive shell.
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

impl ShellArgs {
    pub async fn run(self, output: Output) -> Result<()> {
        if output.is_structured() {
            bail!("frontend shell is a passthrough command and only supports human output");
        }
        let workspace = Workspace::resolve()?;
        let inventory = Inventory::collect(&workspace).await?;
        let (_, runtime) = resolve_observed_guest(&inventory, self.filesystem, self.runtime)?;

        match runtime {
            FrontendRuntime::Docker => {
                let identity = workspace.identity();
                let container_name = frontend_container_name(identity.container_label())?;
                self.exec_in_container(&container_name, output)
            },
            FrontendRuntime::Libkrun => self.exec_in_libkrun_guest(workspace.frontend()),
            FrontendRuntime::Host => unreachable!("validated above"),
        }
    }

    /// Attach to the running FUSE frontend by execing into its guest. The
    /// frontend image supplies `/bin/sh`; `--shell` overrides it and a
    /// trailing command runs non-interactively.
    fn exec_in_container(&self, container_name: &ContainerName, output: Output) -> Result<()> {
        let target = DockerTarget::new(
            container_name.as_str().to_string(),
            FRONTEND_DEV_IMAGE.to_string(),
        )?;
        let runner = DockerRunner::new(DockerClient::connect_for(&target, output)?);
        let cmd = runner.shell_command(self.shell.as_deref(), &self.command);
        spawn_and_propagate(cmd, format!("open shell in container `{container_name}`"))
    }

    /// Attach to the running libkrun guest over ssh-over-vsock.
    fn exec_in_libkrun_guest(&self, frontend: &omnifs_workspace::FrontendState) -> Result<()> {
        libkrun_runner::ensure_socat_available()?;
        let runner = crate::libkrun_runner::LibkrunRunner::new(frontend.libkrun_root());
        let cmd = runner.shell_command(self.shell.as_deref(), &self.command);
        spawn_and_propagate(cmd, "open shell in the libkrun guest".to_string())
    }
}

fn resolve_observed_guest(
    inventory: &Inventory,
    filesystem: Option<FrontendFilesystem>,
    runtime: Option<FrontendRuntime>,
) -> Result<(FrontendFilesystem, FrontendRuntime)> {
    if runtime == Some(FrontendRuntime::Host) {
        bail!(
            "frontend shell is available only for docker and libkrun; host mounts are already available in your ordinary shell"
        );
    }
    if let (Some(filesystem), Some(runtime)) = (filesystem, runtime) {
        ensure!(
            runtime.supports(filesystem),
            "a {filesystem}/{runtime} frontend is not supported on {}",
            std::env::consts::OS
        );
    }

    let matches = inventory
        .frontends
        .iter()
        .filter(|frontend| frontend.runtime != FrontendRuntime::Host)
        .filter(|frontend| filesystem.is_none_or(|value| frontend.filesystem == value))
        .filter(|frontend| runtime.is_none_or(|value| frontend.runtime == value))
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [frontend]
            if matches!(
                frontend.state,
                FrontendState::Attached | FrontendState::Running
            ) =>
        {
            Ok((frontend.filesystem, frontend.runtime))
        },
        [] => {
            let (selection, remedy) = match (filesystem, runtime) {
                (Some(filesystem), Some(runtime)) => (
                    format!("`{filesystem}/{runtime}` frontend"),
                    format!(
                        "Start one with `omnifs frontend enable {filesystem} --runtime {runtime}`."
                    ),
                ),
                (Some(filesystem), None) => (
                    format!("`{filesystem}` guest frontend"),
                    format!("Start one with `omnifs frontend enable {filesystem}`."),
                ),
                (None, Some(runtime)) => (
                    format!("`{runtime}` frontend"),
                    "Run `omnifs frontend ls` to inspect available frontends.".to_owned(),
                ),
                (None, None) => (
                    "guest frontend".to_owned(),
                    "Run `omnifs frontend ls` to inspect available frontends.".to_owned(),
                ),
            };
            bail!("No running {selection} was found. {remedy}")
        },
        [frontend] => bail!(
            "The `{}/{}` frontend failed. Restart it with `omnifs frontend restart {} --runtime {}`.",
            frontend.filesystem,
            frontend.runtime,
            frontend.filesystem,
            frontend.runtime
        ),
        _ => {
            let identities = matches
                .iter()
                .map(|frontend| format!("{}/{}", frontend.filesystem, frontend.runtime))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "frontend shell selection is ambiguous ({identities}); specify the filesystem and --runtime"
            )
        },
    }
}

/// Hand the terminal to `cmd` and forward its exit code so one-shot commands
/// remain scriptable.
fn spawn_and_propagate(mut cmd: Command, context: String) -> Result<()> {
    let status = cmd.status().with_context(|| context)?;
    match status.code() {
        Some(0) | None => Ok(()),
        Some(code) => {
            crate::metrics::record_cli_exit("frontend.shell", code);
            std::process::exit(code)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};
    use std::path::PathBuf;

    use crate::cli::Cli;
    use crate::inventory::FrontendStatus;

    fn inventory_with(state: FrontendState) -> Inventory {
        Inventory {
            home: PathBuf::new(),
            mount_revision: None,
            applied_revision: None,
            daemon: crate::inventory::DaemonObservation {
                status: None,
                probe: crate::inventory::DaemonProbe::Stopped,
                runtime: None,
            },
            frontends: vec![FrontendStatus {
                filesystem: FrontendFilesystem::Fuse,
                runtime: FrontendRuntime::Docker,
                location: Some(PathBuf::from("/omnifs")),
                state,
                scope: "all",
                mount_count: 0,
                fix: None,
            }],
            mounts: Vec::new(),
            warmup: None,
        }
    }

    #[test]
    fn parser_uses_frontend_shell_path_and_trailing_command() {
        let cli = Cli::try_parse_from([
            "omnifs",
            "frontend",
            "shell",
            "fuse",
            "--runtime",
            "docker",
            "--shell",
            "/bin/bash",
            "--",
            "pwd",
        ])
        .unwrap();
        let Some(crate::cli::Commands::Frontend(args)) = cli.command else {
            panic!("expected frontend command");
        };
        let crate::commands::frontend::FrontendCommand::Shell(args) = args.command else {
            panic!("expected frontend shell command");
        };
        assert_eq!(args.filesystem, Some(FrontendFilesystem::Fuse));
        assert_eq!(args.runtime, Some(FrontendRuntime::Docker));
        assert_eq!(args.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(args.command, vec!["pwd"]);

        let cli = Cli::try_parse_from(["omnifs", "frontend", "shell", "fuse"]).unwrap();
        let Some(crate::cli::Commands::Frontend(args)) = cli.command else {
            panic!("expected frontend command");
        };
        let crate::commands::frontend::FrontendCommand::Shell(args) = args.command else {
            panic!("expected frontend shell command");
        };
        assert_eq!(args.filesystem, Some(FrontendFilesystem::Fuse));
        assert_eq!(args.runtime, None);

        let cli = Cli::try_parse_from(["omnifs", "frontend", "shell"]).unwrap();
        let Some(crate::cli::Commands::Frontend(args)) = cli.command else {
            panic!("expected frontend command");
        };
        let crate::commands::frontend::FrontendCommand::Shell(args) = args.command else {
            panic!("expected frontend shell command");
        };
        assert_eq!(args.filesystem, None);
        assert_eq!(args.runtime, None);

        let command = Cli::command();
        let frontend = command
            .find_subcommand("frontend")
            .expect("frontend command")
            .clone();
        assert!(frontend.find_subcommand("shell").is_some());
        assert!(command.find_subcommand("shell").is_none());
    }

    #[test]
    fn observed_selection_accepts_attached_or_running() {
        for state in [FrontendState::Attached, FrontendState::Running] {
            assert!(
                resolve_observed_guest(
                    &inventory_with(state),
                    Some(FrontendFilesystem::Fuse),
                    None,
                )
                .is_ok_and(|identity| {
                    identity == (FrontendFilesystem::Fuse, FrontendRuntime::Docker)
                })
            );
        }
    }

    #[test]
    fn observed_selection_infers_the_only_guest_and_ignores_host_mounts() {
        let mut inventory = inventory_with(FrontendState::Attached);
        inventory.frontends.push(FrontendStatus {
            filesystem: FrontendFilesystem::Nfs,
            runtime: FrontendRuntime::Host,
            location: Some(PathBuf::from("/tmp/omnifs")),
            state: FrontendState::Attached,
            scope: "all",
            mount_count: 0,
            fix: None,
        });

        assert_eq!(
            resolve_observed_guest(&inventory, None, None).unwrap(),
            (FrontendFilesystem::Fuse, FrontendRuntime::Docker)
        );
    }

    #[test]
    fn observed_selection_rejects_absent_failed_and_ambiguous() {
        let absent = Inventory {
            frontends: Vec::new(),
            ..inventory_with(FrontendState::Attached)
        };
        let error = resolve_observed_guest(
            &absent,
            Some(FrontendFilesystem::Fuse),
            Some(FrontendRuntime::Docker),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "No running `fuse/docker` frontend was found. Start one with `omnifs frontend enable fuse --runtime docker`."
        );

        let failed = resolve_observed_guest(
            &inventory_with(FrontendState::Failed),
            Some(FrontendFilesystem::Fuse),
            Some(FrontendRuntime::Docker),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            failed,
            "The `fuse/docker` frontend failed. Restart it with `omnifs frontend restart fuse --runtime docker`."
        );

        let mut ambiguous = inventory_with(FrontendState::Attached);
        ambiguous.frontends.push(FrontendStatus {
            filesystem: FrontendFilesystem::Fuse,
            runtime: FrontendRuntime::Docker,
            location: Some(PathBuf::from("/omnifs-2")),
            state: FrontendState::Running,
            scope: "all",
            mount_count: 0,
            fix: None,
        });
        let error = resolve_observed_guest(&ambiguous, Some(FrontendFilesystem::Fuse), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"));
    }

    #[test]
    fn observed_selection_reports_an_unsupported_pair_before_shell_support() {
        let error = resolve_observed_guest(
            &inventory_with(FrontendState::Attached),
            Some(FrontendFilesystem::Nfs),
            Some(FrontendRuntime::Libkrun),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("nfs/libkrun"));
        assert!(error.contains("not supported"));
        assert!(!error.contains("only the fuse filesystem"));
    }
}
