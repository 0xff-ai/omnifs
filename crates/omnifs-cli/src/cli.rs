#![allow(clippy::disallowed_macros)] // migrates in wave 4 (cli-redesign)
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

    /// Progress rendering. `text` animates a stderr row (the default); `json`
    /// emits one NDJSON `UiEvent` per line on stdout for machine consumers.
    #[arg(long, global = true, value_enum, default_value_t = ProgressFormat::Text)]
    pub progress: ProgressFormat,

    /// Suppress conversational narration on stderr. Receipts, progress settle
    /// lines, and errors are preserved.
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Progress stream selection for the global `--progress` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ProgressFormat {
    Text,
    Json,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Show daemon, mount, and auth status
    Status(commands::status::StatusArgs),

    /// Start the daemon and serve configured mounts
    ///
    /// Spawns the host-native daemon over configured mounts, then launches
    /// every frontend in the effective `[[frontends]]` plan (explicit config,
    /// else the platform default). `--no-frontend` starts the daemon only,
    /// on every OS.
    Up(commands::up::UpArgs),
    /// Stop the daemon and clean up
    ///
    /// Tears down every running frontend (local, Docker, or krunkit), then
    /// the daemon.
    Down(commands::down::DownArgs),
    /// Tail the daemon log
    Logs(commands::logs::LogsArgs),
    /// Stream FUSE, provider, and callout events
    Inspect(commands::inspect::InspectArgs),
    /// Open a shell at the projected tree
    ///
    /// The mount surface (Docker-hosted frontend or host-native mount) comes
    /// from live frontend state. A local mount can be selected explicitly.
    /// Use `--mount NAME` to select a local frontend mount by basename or exact
    /// path, bypassing guest frontend preference when more than one surface is
    /// available.
    Shell(commands::shell::ShellArgs),

    /// Guided setup: environment, providers, auth, launch
    ///
    /// First-run wizard: detect the OS, pick several providers, authenticate
    /// each, and launch in one pass.
    ///
    /// Run this once to get started; use `omnifs mount add` to add a single
    /// provider later. Re-runnable: already-configured providers are listed
    /// but excluded from the picker.
    Setup(commands::setup::SetupArgs),

    /// Add, list, reauthenticate, snapshot, or remove mounts
    Mount(commands::mount::MountArgs),

    /// List or install provider artifacts
    Provider(commands::provider::ProviderArgs),

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
    /// Prints the one-line build identity. Use `--json` for structured CLI,
    /// daemon, channel, and provider facts.
    Version(commands::version::VersionArgs),

    /// Debug utilities. Hidden from `--help`.
    #[command(hide = true)]
    Debug(commands::debug::DebugArgs),

    /// Run the runtime daemon. Internal: launched by the host-native lifecycle
    /// command, not invoked directly. The daemon still runs as its own process
    /// over the control API; this is the same binary, not a separate entrypoint.
    #[command(hide = true)]
    #[cfg(feature = "daemon")]
    Daemon(omnifs_daemon::DaemonArgs),

    /// Manage the optional Docker-hosted FUSE frontend attached to a
    /// host-native daemon
    ///
    /// The daemon always runs host-native. `omnifs frontend up` launches a
    /// separate, credential-free Docker container that renders FUSE over the
    /// daemon's shared namespace; `down` tears it down.
    Frontend(commands::frontend::FrontendArgs),
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
            Self::Setup(_) => "setup",
            Self::Mount(_) => "mount",
            Self::Provider(_) => "provider",
            Self::Skill(_) => "skill",
            Self::Reset(_) => "reset",
            Self::Doctor(_) => "doctor",
            Self::Completions(_) => "completions",
            Self::Version(_) => "version",
            Self::Debug(_) => "debug",
            #[cfg(feature = "daemon")]
            Self::Daemon(_) => return None,
            // Every `frontend` subcommand shares one telemetry label; there is
            // no longer a hidden internal one (like `daemon`'s) to exclude.
            Self::Frontend(_) => "frontend",
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
            Self::Up(args) => args.run().await,
            Self::Down(args) => args.run().await,
            Self::Logs(args) => args.run().map(|()| ExitCode::Success),
            Self::Inspect(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Shell(args) => args.run().await.map(|()| ExitCode::Success),
            Self::Mount(args) => args.run().await,
            Self::Provider(args) => args.run().await,
            Self::Skill(args) => args.run().map(|()| ExitCode::Success),
            Self::Reset(args) => args.run().await,
            Self::Completions(args) => {
                args.run();
                Ok(ExitCode::Success)
            },
            Self::Version(args) => args.run().await,
            Self::Debug(args) => args.run().map(|()| ExitCode::Success),
            #[cfg(feature = "daemon")]
            Self::Daemon(args) => omnifs_daemon::run(&args).map(|()| ExitCode::Success),
            Self::Frontend(args) => args.run().await,
        }
    }
}

/// Bare `omnifs` adapts to the workspace: an unconfigured workspace points at
/// `setup`; a configured-but-stopped daemon shows the status report plus an
/// `up` hint; a healthy daemon shows the full status report plus two next-step
/// hints. It is a thin dispatcher over the shared status/report code, so it
/// never drifts from `omnifs status`.
async fn run_bare() -> anyhow::Result<ExitCode> {
    let workspace = Workspace::resolve()?;
    let mounts = workspace.mounts().unwrap_or_default();
    if mounts.is_empty() {
        anstream::println!("omnifs is not set up. Run `omnifs setup` to get started.");
        return Ok(ExitCode::Success);
    }

    let runtime = workspace.daemon().compatible_status_optional().await?;
    let report = crate::status::StatusReport::collect(
        workspace.catalog(),
        workspace.layout().clone(),
        runtime,
        &mounts,
    );
    let exit_code = report.exit_code();
    let running = report.runtime.is_some();
    report.build_report(false).print();

    // The status report is the record (stdout); these next-step hints are
    // conversational, so `-q` drops them.
    if running {
        crate::ui::narrate("");
        crate::ui::narrate(crate::ui::hint("omnifs shell", "open a shell at the tree"));
        crate::ui::narrate(crate::ui::hint(
            "omnifs mount add <provider>",
            "add another mount",
        ));
    } else {
        crate::ui::narrate("");
        crate::ui::narrate(crate::ui::hint("omnifs up", "start the daemon"));
    }
    Ok(exit_code)
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
            ("setup provider picker", "setup", "providers"),
            ("setup provider confirmation", "setup", "yes"),
            ("mount add provider picker", "mount add", "provider"),
            ("mount add mount name collision", "mount add", "as"),
            ("mount add auth scheme", "mount add", "scheme"),
            ("mount add OAuth browser", "mount add", "no-browser"),
            ("mount add static token", "mount add", "token-env"),
            ("mount add auth suppression", "mount add", "no-auth"),
            ("mount add provider config", "mount add", "config-json"),
            (
                "mount add capability grants",
                "mount add",
                "capabilities-json",
            ),
            ("mount add resource limits", "mount add", "limits-json"),
            ("up readiness wait", "up", "wait"),
        ];

        for (prompt, subcommand, arg) in table {
            assert!(
                has_arg(&command, subcommand, arg),
                "prompt site `{prompt}` must be covered by `{subcommand}` arg `{arg}`"
            );
        }
    }

    /// Resolve a whitespace-separated subcommand path (for example `mount add`)
    /// and check whether the leaf subcommand declares the argument.
    fn has_arg(command: &clap::Command, subcommand: &str, arg: &str) -> bool {
        let mut current = command;
        for segment in subcommand.split_whitespace() {
            let Some(next) = current.find_subcommand(segment) else {
                return false;
            };
            current = next;
        }
        current
            .get_arguments()
            .any(|candidate| candidate.get_id() == arg || candidate.get_long() == Some(arg))
    }
}
