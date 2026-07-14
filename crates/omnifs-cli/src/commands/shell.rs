//! `omnifs frontend shell`: enter one observed guest frontend.

use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use clap::Args;

use crate::commands::frontend::{FrontendEnvironment, FrontendFilesystem};
use crate::frontend_backend::{DockerBackend, FrontendBackend};
use crate::frontend_container::FRONTEND_DEV_IMAGE;
use crate::inventory::{FrontendState, Inventory};
use crate::krunkit_backend::{self, KrunkitBackend};
use crate::launch_backend::{ContainerName, DockerTarget};
use crate::runtime::Runtime;
use crate::ui::output::Output;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone)]
pub struct ShellArgs {
    /// Filesystem exposed by the frontend.
    #[arg(value_enum)]
    pub filesystem: FrontendFilesystem,
    /// Guest environment hosting the frontend.
    #[arg(long, value_enum)]
    pub environment: FrontendEnvironment,
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
        ensure!(
            self.filesystem == FrontendFilesystem::Fuse,
            "frontend shell currently supports only the fuse filesystem"
        );
        ensure!(
            matches!(
                self.environment,
                FrontendEnvironment::Docker | FrontendEnvironment::Krunkit
            ),
            "frontend shell is available only for docker and krunkit; host mounts are already available in your ordinary shell"
        );

        let workspace = Workspace::resolve()?;
        let inventory = Inventory::collect(&workspace).await?;
        ensure_observed_guest(&inventory, self.filesystem, self.environment)?;

        let paths = workspace.layout();
        match self.environment {
            FrontendEnvironment::Docker => {
                let container_name = crate::frontend_container::frontend_container_name(paths)?;
                self.exec_in_container(&container_name, output)
            },
            FrontendEnvironment::Krunkit => self.exec_in_krunkit_guest(paths),
            FrontendEnvironment::Host => unreachable!("validated above"),
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
        let backend = DockerBackend::new(Runtime::connect_for(&target, output)?);
        let cmd = backend.shell_command(self.shell.as_deref(), &self.command);
        spawn_and_propagate(cmd, format!("open shell in container `{container_name}`"))
    }

    /// Attach to the running krunkit guest over ssh-over-vsock.
    fn exec_in_krunkit_guest(
        &self,
        paths: &omnifs_workspace::layout::WorkspaceLayout,
    ) -> Result<()> {
        krunkit_backend::ensure_socat_available()?;
        let backend = KrunkitBackend::new(paths.config_dir.clone());
        let cmd = backend.shell_command(self.shell.as_deref(), &self.command);
        spawn_and_propagate(cmd, "open shell in the krunkit guest".to_string())
    }
}

fn ensure_observed_guest(
    inventory: &Inventory,
    filesystem: FrontendFilesystem,
    environment: FrontendEnvironment,
) -> Result<()> {
    let identity = format!("{filesystem}/{environment}");
    let remedy = format!("omnifs frontend enable {filesystem} --environment {environment}");
    let matches = inventory
        .frontends
        .iter()
        .filter(|frontend| frontend.filesystem == filesystem && frontend.environment == environment)
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [frontend]
            if matches!(
                frontend.state,
                FrontendState::Attached | FrontendState::Running
            ) =>
        {
            Ok(())
        },
        [] => bail!("frontend `{identity}` is not observed; start it with `{remedy}`"),
        [_] => bail!("frontend `{identity}` is observed but failed; restart it or run `{remedy}`"),
        _ => bail!(
            "frontend `{identity}` is ambiguous in observed state; stop duplicates and run `{remedy}`"
        ),
    }
}

/// Hand the terminal to `cmd` and forward its exit code so one-shot commands
/// remain scriptable.
fn spawn_and_propagate(mut cmd: Command, context: String) -> Result<()> {
    let status = cmd.status().with_context(|| context)?;
    match status.code() {
        Some(0) | None => Ok(()),
        Some(code) => {
            crate::telemetry::record_cli_exit("frontend.shell", code);
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
            desired_mounts: Vec::new(),
            daemon: crate::inventory::DaemonObservation {
                status: None,
                probe: crate::inventory::DaemonProbe::Stopped,
                runtime: None,
            },
            runners: Vec::new(),
            frontends: vec![FrontendStatus {
                filesystem: FrontendFilesystem::Fuse,
                environment: FrontendEnvironment::Docker,
                location: Some(PathBuf::from("/omnifs")),
                state,
                scope: "all",
                mount_count: 0,
                fix: None,
            }],
            mounts: Vec::new(),
            providers: Vec::new(),
            startup_credentials: Vec::new(),
        }
    }

    #[test]
    fn parser_uses_frontend_shell_path_and_trailing_command() {
        let cli = Cli::try_parse_from([
            "omnifs",
            "frontend",
            "shell",
            "fuse",
            "--environment",
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
        assert_eq!(args.filesystem, FrontendFilesystem::Fuse);
        assert_eq!(args.environment, FrontendEnvironment::Docker);
        assert_eq!(args.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(args.command, vec!["pwd"]);

        let command = Cli::command();
        let frontend = command
            .find_subcommand("frontend")
            .expect("frontend command")
            .clone();
        assert!(frontend.find_subcommand("shell").is_some());
        assert!(command.find_subcommand("shell").is_none());
    }

    #[test]
    fn exact_observed_selection_accepts_attached_or_running() {
        for state in [FrontendState::Attached, FrontendState::Running] {
            assert!(
                ensure_observed_guest(
                    &inventory_with(state),
                    FrontendFilesystem::Fuse,
                    FrontendEnvironment::Docker
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn exact_observed_selection_rejects_absent_failed_and_ambiguous() {
        let absent = Inventory {
            frontends: Vec::new(),
            ..inventory_with(FrontendState::Attached)
        };
        let error = ensure_observed_guest(
            &absent,
            FrontendFilesystem::Fuse,
            FrontendEnvironment::Docker,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("fuse/docker"));
        assert!(error.contains("omnifs frontend enable fuse --environment docker"));

        let failed = ensure_observed_guest(
            &inventory_with(FrontendState::Failed),
            FrontendFilesystem::Fuse,
            FrontendEnvironment::Docker,
        )
        .unwrap_err()
        .to_string();
        assert!(failed.contains("failed"));

        let mut ambiguous = inventory_with(FrontendState::Attached);
        ambiguous.frontends.push(FrontendStatus {
            filesystem: FrontendFilesystem::Fuse,
            environment: FrontendEnvironment::Docker,
            location: Some(PathBuf::from("/omnifs-2")),
            state: FrontendState::Running,
            scope: "all",
            mount_count: 0,
            fix: None,
        });
        let error = ensure_observed_guest(
            &ambiguous,
            FrontendFilesystem::Fuse,
            FrontendEnvironment::Docker,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("ambiguous"));
    }
}
