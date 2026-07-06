//! CLI type definitions: top-level parser and command enum.

use clap::{Parser, Subcommand};

use crate::commands;
use crate::commands::doctor::DoctorVerdict;
use crate::error::ExitCode;
use crate::workspace::Workspace;

#[derive(Parser)]
#[command(
    name = "omnifs",
    version,
    about = "omnifs: a virtual filesystem for everything",
    after_help = "Exit codes:\n  0  success\n  1  generic failure\n  2  usage error\n  3  daemon unreachable\n  4  auth or consent required\n  5  degraded health"
)]
pub struct Cli {
    /// Increase tracing verbosity. -v = info, -vv = debug with span events.
    /// Overridden by `RUST_LOG`.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Show daemon, mount, and auth status
    Status(commands::status::StatusArgs),

    /// Start the daemon and serve configured mounts
    ///
    /// Materializes host credentials into a session dir, hands them to the
    /// runtime backend, and launches it. Under Docker this bind-mounts the
    /// session dir into the container.
    Up(commands::up::UpArgs),
    /// Stop the daemon and clean up
    ///
    /// Tears down the running backend and removes its session dir. Under
    /// Docker this also removes the container.
    Down(commands::down::DownArgs),
    /// Tail the daemon log
    Logs(commands::logs::LogsArgs),
    /// Stream FUSE, provider, and callout events
    Inspect(commands::inspect::InspectArgs),
    /// Open a shell at the projected tree
    ///
    /// The daemon mode and mount point come from the run-state file
    /// `omnifs up` wrote.
    Shell(commands::shell::ShellArgs),

    /// Export a mount's canonical cache to a directory
    Snapshot(commands::snapshot::SnapshotArgs),

    /// Guided setup: runtime, providers, auth, launch
    ///
    /// First-run wizard: detect the OS, explain the runtime, pick several
    /// providers, authenticate each, and launch in one pass.
    ///
    /// Run this once to get started; use `omnifs init` to add a single
    /// provider later. Re-runnable: already-configured providers are listed
    /// but excluded from the picker.
    Setup(commands::setup::SetupArgs),

    /// Add and authenticate a mount
    Init(commands::init::InitArgs),

    /// List, reauthenticate, or remove mounts
    Mounts(commands::mounts::MountsArgs),

    /// List or install provider artifacts
    Providers(commands::providers::ProvidersArgs),

    /// Install omnifs usage skills for agent harnesses
    Skill(commands::skill::SkillArgs),

    /// Remove all mounts and credentials, stop the daemon
    ///
    /// Nukes every mount config and (by default) its stored credential, then
    /// stops the running backend. Asks for confirmation unless `--yes` is set.
    Reset(commands::reset::ResetArgs),

    /// Diagnose environment, auth, and daemon health
    Doctor(commands::doctor::DoctorArgs),

    /// Print shell completions
    Completions(commands::completions::CompletionsArgs),

    /// Print version information
    ///
    /// Use `--detail` for daemon, store, and provider count alongside the CLI
    /// version.
    Version(commands::version::VersionArgs),

    /// Debug utilities. Hidden from `--help`.
    #[command(hide = true)]
    Debug(commands::debug::DebugArgs),

    /// Run the runtime daemon. Internal: launched by the container
    /// entrypoint (and, later, the host-native launcher), not invoked
    /// directly. The daemon still runs as its own process over the
    /// control API; this is the same binary, not a separate entrypoint.
    #[command(hide = true)]
    #[cfg(feature = "daemon")]
    Daemon(omnifs_daemon::DaemonArgs),
}

/// Human (`Text`) vs machine (`Json`) output selection, shared by commands that
/// support `--json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputFormat {
    Text,
    Json,
}

impl From<bool> for OutputFormat {
    fn from(json: bool) -> Self {
        if json { Self::Json } else { Self::Text }
    }
}

impl Cli {
    pub(crate) fn telemetry_label(&self) -> Option<&'static str> {
        self.command
            .as_ref()
            .map_or(Some("bare"), Commands::telemetry_label)
    }

    pub async fn run(self) -> anyhow::Result<ExitCode> {
        match self.command {
            Some(command) => command.run().await,
            None => run_bare().await,
        }
    }
}

impl Commands {
    /// Top-level subcommand label for `cli.jsonl` telemetry, or `None` for the
    /// internal `daemon` subcommand (which records `daemon.jsonl` instead of
    /// counting as CLI usage).
    pub(crate) fn telemetry_label(&self) -> Option<&'static str> {
        Some(match self {
            Self::Status(_) => "status",
            Self::Up(_) => "up",
            Self::Down(_) => "down",
            Self::Logs(_) => "logs",
            Self::Inspect(_) => "inspect",
            Self::Shell(_) => "shell",
            Self::Snapshot(_) => "snapshot",
            Self::Setup(_) => "setup",
            Self::Init(_) => "init",
            Self::Mounts(_) => "mounts",
            Self::Providers(_) => "providers",
            Self::Skill(_) => "skill",
            Self::Reset(_) => "reset",
            Self::Doctor(_) => "doctor",
            Self::Completions(_) => "completions",
            Self::Version(_) => "version",
            Self::Debug(_) => "debug",
            #[cfg(feature = "daemon")]
            Self::Daemon(_) => return None,
        })
    }

    pub async fn run(self) -> anyhow::Result<ExitCode> {
        match self {
            Self::Doctor(args) => {
                let verdict = args.run().await?;
                Ok(exit_for_verdict(verdict))
            },
            Self::Status(args) => args.run().await,
            Self::Setup(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Init(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Up(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Down(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Logs(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Inspect(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Shell(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Snapshot(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Mounts(args) => args.run().await,
            Self::Providers(args) => args.run().await,
            Self::Skill(args) => args.run().map(|()| ExitCode::Success),
            Self::Reset(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Completions(args) => {
                args.run();
                Ok(ExitCode::Success)
            },
            Self::Version(args) => args.run().await,
            Self::Debug(args) => args.run().map(|()| ExitCode::Success),
            #[cfg(feature = "daemon")]
            Self::Daemon(args) => omnifs_daemon::run(args).map(|()| ExitCode::Success),
        }
    }
}

async fn run_bare() -> anyhow::Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let configured = workspace
        .config()
        .is_ok_and(|config| config.system.runtime.is_some())
        || workspace.mounts().is_ok_and(|mounts| !mounts.is_empty());

    if configured {
        commands::status::StatusArgs::default().run().await
    } else {
        anstream::println!("omnifs is not set up. Run `omnifs setup` to get started.");
        Ok(ExitCode::Success)
    }
}

fn exit_for_verdict(verdict: DoctorVerdict) -> ExitCode {
    match verdict {
        DoctorVerdict::Clean => ExitCode::Success,
        DoctorVerdict::Failures => ExitCode::GenericFailure,
        DoctorVerdict::Warnings => ExitCode::Degraded,
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn wizard_prompt_sites_have_non_interactive_flags() {
        let command = Cli::command();
        let table = [
            ("setup orientation", "setup", "yes"),
            ("setup environment check", "setup", "no-input"),
            ("setup runtime selection", "setup", "runtime"),
            ("setup mount point", "setup", "mount-point"),
            ("setup provider picker", "setup", "providers"),
            ("setup provider confirmation", "setup", "yes"),
            ("init provider picker", "init", "provider"),
            ("init mount name collision", "init", "as"),
            ("init auth scheme", "init", "scheme"),
            ("init OAuth browser", "init", "no-browser"),
            ("init static token", "init", "token-env"),
            ("init auth suppression", "init", "no-auth"),
            ("init provider config", "init", "config-json"),
            ("init capability grants", "init", "capabilities-json"),
            ("init resource limits", "init", "limits-json"),
            ("up readiness wait", "up", "wait"),
        ];

        for (prompt, subcommand, arg) in table {
            assert!(
                has_arg(&command, subcommand, arg),
                "prompt site `{prompt}` must be covered by `{subcommand}` arg `{arg}`"
            );
        }
    }

    fn has_arg(command: &clap::Command, subcommand: &str, arg: &str) -> bool {
        let Some(command) = command.find_subcommand(subcommand) else {
            return false;
        };
        command
            .get_arguments()
            .any(|candidate| candidate.get_id() == arg || candidate.get_long() == Some(arg))
    }
}
