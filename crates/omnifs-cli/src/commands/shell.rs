//! `omnifs frontend shell`: enter one observed guest frontend.

use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use omnifs_api::{FrontendRuntime, FsType};

use crate::commands::frontend::{frontend_runtime_parser, fs_type_parser, supports};
use crate::docker::{ContainerName, DockerClient, DockerTarget};
use crate::frontend_container::{FRONTEND_DEV_IMAGE, frontend_container_name};
use crate::libkrun_runner;
use crate::ui::output::Output;
use omnifs_workspace::Workspace;

#[derive(Args, Debug, Clone)]
pub struct ShellArgs {
    /// Filesystem exposed by the frontend.
    #[arg(value_parser = fs_type_parser())]
    pub filesystem: Option<FsType>,
    /// Guest runtime hosting the frontend.
    #[arg(long, value_parser = frontend_runtime_parser())]
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
        let probes = probe_guests(&workspace, output.clone()).await;
        let (_, runtime) = resolve_observed_guest(&probes, self.filesystem, self.runtime)?;

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
        let client = DockerClient::connect_for(&target, output)?;
        let cmd = client.shell_command(self.shell.as_deref(), &self.command);
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

#[derive(Debug, Clone)]
struct GuestProbe {
    filesystem: FsType,
    runtime: FrontendRuntime,
    running: Result<bool, String>,
}

async fn probe_guests(workspace: &Workspace, output: Output) -> Vec<GuestProbe> {
    let mut probes = Vec::new();
    if supports(FsType::Fuse, FrontendRuntime::Docker) {
        let running = async {
            let config = workspace.config()?;
            let image = crate::frontend_container::resolve_frontend_image(None, &config)?;
            let name = frontend_container_name(workspace.identity().container_label())?;
            let target = DockerTarget::new(name.as_str().to_owned(), image.as_str().to_owned())?;
            DockerClient::connect_for(&target, output)?
                .is_running()
                .await
                .map(Option::unwrap_or_default)
        }
        .await
        .map_err(|error: anyhow::Error| format!("{error:#}"));
        probes.push(GuestProbe {
            filesystem: FsType::Fuse,
            runtime: FrontendRuntime::Docker,
            running,
        });
    }
    if supports(FsType::Fuse, FrontendRuntime::Libkrun) {
        let running =
            crate::libkrun_runner::LibkrunRunner::new(workspace.frontend().libkrun_root())
                .is_running()
                .map(Option::unwrap_or_default)
                .map_err(|error| format!("{error:#}"));
        probes.push(GuestProbe {
            filesystem: FsType::Fuse,
            runtime: FrontendRuntime::Libkrun,
            running,
        });
    }
    probes
}

fn resolve_observed_guest(
    probes: &[GuestProbe],
    filesystem: Option<FsType>,
    runtime: Option<FrontendRuntime>,
) -> Result<(FsType, FrontendRuntime)> {
    if runtime == Some(FrontendRuntime::Host) {
        bail!(
            "frontend shell is available only for docker and libkrun; host mounts are already available in your ordinary shell"
        );
    }
    if let (Some(filesystem), Some(runtime)) = (filesystem, runtime) {
        ensure!(
            supports(filesystem, runtime),
            "a {filesystem}/{runtime} frontend is not supported on {}",
            std::env::consts::OS
        );
    }
    let matches = probes
        .iter()
        .filter(|probe| filesystem.is_none_or(|value| probe.filesystem == value))
        .filter(|probe| runtime.is_none_or(|value| probe.runtime == value))
        .filter(|probe| probe.running.as_ref().is_ok_and(|running| *running))
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [frontend] => Ok((frontend.filesystem, frontend.runtime)),
        [] => {
            let errors = probes
                .iter()
                .filter(|probe| filesystem.is_none_or(|value| probe.filesystem == value))
                .filter(|probe| runtime.is_none_or(|value| probe.runtime == value))
                .filter_map(|probe| {
                    probe
                        .running
                        .as_ref()
                        .err()
                        .map(|error| format!("{}/{}: {error}", probe.filesystem, probe.runtime))
                })
                .collect::<Vec<_>>();
            if !errors.is_empty() {
                bail!(
                    "Could not inspect the selected guest frontend: {}",
                    errors.join("; ")
                );
            }
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

    use crate::cli::Cli;

    fn probe(runtime: FrontendRuntime, running: Result<bool, &str>) -> GuestProbe {
        GuestProbe {
            filesystem: FsType::Fuse,
            runtime,
            running: running.map_err(str::to_owned),
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
        assert_eq!(args.filesystem, Some(FsType::Fuse));
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
        assert_eq!(args.filesystem, Some(FsType::Fuse));
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
    fn observed_selection_accepts_a_confirmed_runtime() {
        assert_eq!(
            resolve_observed_guest(
                &[probe(FrontendRuntime::Docker, Ok(true))],
                Some(FsType::Fuse),
                None,
            )
            .unwrap(),
            (FsType::Fuse, FrontendRuntime::Docker)
        );
    }

    #[test]
    fn one_failed_probe_does_not_block_one_confirmed_runtime() {
        assert_eq!(
            resolve_observed_guest(
                &[
                    probe(FrontendRuntime::Docker, Err("docker unavailable")),
                    probe(FrontendRuntime::Libkrun, Ok(true)),
                ],
                None,
                None,
            )
            .unwrap(),
            (FsType::Fuse, FrontendRuntime::Libkrun)
        );
        assert_eq!(
            resolve_observed_guest(
                &[
                    probe(FrontendRuntime::Docker, Ok(true)),
                    probe(FrontendRuntime::Libkrun, Err("helper unavailable")),
                ],
                None,
                None,
            )
            .unwrap(),
            (FsType::Fuse, FrontendRuntime::Docker)
        );
    }

    #[test]
    fn observed_selection_rejects_absent_failed_and_ambiguous() {
        let error = resolve_observed_guest(
            &[probe(FrontendRuntime::Docker, Ok(false))],
            Some(FsType::Fuse),
            Some(FrontendRuntime::Docker),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "No running `fuse/docker` frontend was found. Start one with `omnifs frontend enable fuse --runtime docker`."
        );

        let failed = resolve_observed_guest(
            &[probe(FrontendRuntime::Docker, Err("daemon unavailable"))],
            Some(FsType::Fuse),
            Some(FrontendRuntime::Docker),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            failed,
            "Could not inspect the selected guest frontend: fuse/docker: daemon unavailable"
        );

        let error = resolve_observed_guest(
            &[
                probe(FrontendRuntime::Docker, Ok(true)),
                probe(FrontendRuntime::Libkrun, Ok(true)),
            ],
            Some(FsType::Fuse),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("ambiguous"));
    }

    #[test]
    fn observed_selection_reports_an_unsupported_pair_before_shell_support() {
        let error = resolve_observed_guest(
            &[probe(FrontendRuntime::Libkrun, Ok(true))],
            Some(FsType::Nfs),
            Some(FrontendRuntime::Libkrun),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("nfs/libkrun"));
        assert!(error.contains("not supported"));
        assert!(!error.contains("only the fuse filesystem"));
    }
}
