//! CLI type definitions: top-level parser and command enum.

use clap::{Parser, Subcommand};

use crate::commands;
use crate::commands::doctor::DoctorVerdict;

#[derive(Parser)]
#[command(
    name = "omnifs",
    version,
    about = "omnifs: a virtual filesystem for everything"
)]
pub struct Cli {
    /// Increase tracing verbosity. -v = info, -vv = debug with span events.
    /// Overridden by `RUST_LOG`.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Print mount and provider configuration status.
    Status(commands::status::StatusArgs),

    /// Start omnifs with your configured mounts: materialize host credentials
    /// into a session dir, bind-mount them into the container, then run it.
    Up(commands::up::UpArgs),
    /// Stop and remove the omnifs container and clean up the session dir.
    Down(commands::down::DownArgs),
    /// Tail the daemon log inside the container.
    Logs(commands::logs::LogsArgs),
    /// Inspector stream: FUSE, provider, and callout JSONL events.
    Inspect(commands::inspect::InspectArgs),
    /// Open an omnifs-aware shell for exploring the projected tree. The daemon
    /// mode and mount point come from the run-state file `omnifs up` wrote.
    Shell(commands::shell::ShellArgs),

    /// First-run wizard: detect OS, explain Docker, pick several providers,
    /// authenticate each, and launch the container in one pass.
    ///
    /// Run this once to get started; use `omnifs mounts add` (or `omnifs init`)
    /// to add a single provider later. Re-runnable: already-configured
    /// providers are listed but excluded from the picker.
    Setup(commands::setup::SetupArgs),

    /// Interactive setup for a new mount (alias for `omnifs mounts add`).
    Init(commands::init::InitArgs),

    /// Manage configured mounts: add, ls, rm.
    Mounts(commands::mounts::MountsArgs),

    /// Manage installed provider WASM artifacts.
    Providers(commands::providers::ProvidersArgs),

    /// Nuke every mount config and (by default) its stored credential,
    /// then stop and remove the container. Asks for confirmation unless
    /// `--yes` is set.
    Reset(commands::reset::ResetArgs),

    /// Diagnose environment and auth.
    Doctor(commands::doctor::DoctorArgs),

    /// Print shell completions.
    Completions(commands::completions::CompletionsArgs),

    /// Print version information. Use --detail for daemon / store /
    /// provider count alongside the CLI version.
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

impl Commands {
    pub async fn run(self) -> anyhow::Result<()> {
        match self {
            Self::Doctor(args) => {
                let verdict = args.run().await?;
                exit_for_verdict(verdict);
            },
            Self::Status(args) => args.run().await,
            Self::Setup(args) => args.run().await,
            Self::Init(args) => args.run().await,
            Self::Up(args) => args.run().await,
            Self::Down(args) => args.run().await,
            Self::Logs(args) => args.run().await,
            Self::Inspect(args) => args.run().await,
            Self::Shell(args) => args.run().await,
            Self::Mounts(args) => args.run().await,
            Self::Providers(args) => args.run(),
            Self::Reset(args) => args.run().await,
            Self::Completions(args) => {
                args.run();
                Ok(())
            },
            Self::Version(args) => args.run().await,
            Self::Debug(args) => args.run(),
            #[cfg(feature = "daemon")]
            Self::Daemon(args) => omnifs_daemon::run(args),
        }
    }
}

fn exit_for_verdict(verdict: DoctorVerdict) -> ! {
    std::process::exit(match verdict {
        DoctorVerdict::Clean => 0,
        DoctorVerdict::Failures => 1,
        DoctorVerdict::Warnings => 2,
    })
}
