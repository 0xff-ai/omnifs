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

    /// Manage provider credentials.
    Auth(commands::auth::AuthArgs),

    /// Bring up the omnifs container: materialize credentials from the
    /// host credential store, write them to a per-session directory,
    /// bind-mount the rewritten configs into the container, then start it.
    Up(commands::up::UpArgs),
    /// Bring up the canonical local dev sandbox: build the image, wire host
    /// credentials, expose the Docker socket + DB fixture, and start the
    /// container with all built-in providers' dev mounts. Source checkout
    /// required.
    Dev(commands::dev::DevArgs),
    /// Stop and remove the omnifs container and clean up the session dir.
    Down(commands::down::DownArgs),
    /// Tail the daemon log inside the container.
    Logs(commands::logs::LogsArgs),
    /// Inspector stream: FUSE, provider, and callout JSONL events.
    Inspect(commands::inspect::InspectArgs),
    /// Open an interactive shell inside the running container.
    Shell(commands::shell::ShellArgs),

    /// Guided onboarding walkthrough: detect OS, explain Docker, pick
    /// providers, run init per provider, launch the container.
    ///
    /// Re-runnable. Already-configured providers are listed but excluded
    /// from the picker.
    Setup(commands::setup::SetupArgs),

    /// Interactive setup for a new mount.
    Init(commands::init::InitArgs),

    /// Manage configured mounts.
    Mounts(commands::mounts::MountsArgs),

    /// Nuke every mount config and (by default) its stored credential,
    /// then stop and remove the container. Asks for confirmation unless
    /// `--yes` is set.
    Reset(commands::reset::ResetArgs),

    /// Diagnose environment and auth.
    Doctor(commands::doctor::DoctorArgs),

    /// Print shell completions.
    Completions(commands::completions::CompletionsArgs),

    /// Print version information. Use --detail for image / container /
    /// store / provider count alongside the CLI version.
    Version(commands::version::VersionArgs),

    /// Debug utilities. Hidden from `--help`.
    #[command(hide = true)]
    Debug(commands::debug::DebugArgs),
}

impl Commands {
    pub async fn run(self) -> anyhow::Result<()> {
        match self {
            Self::Doctor(args) => {
                let verdict = args.run().await?;
                exit_for_verdict(verdict);
            },
            Self::Status(args) => args.run().await,
            Self::Auth(args) => args.run().await,
            Self::Setup(args) => args.run().await,
            Self::Init(args) => args.run().await,
            Self::Up(args) => args.run().await,
            Self::Dev(args) => args.run().await,
            Self::Down(args) => args.run().await,
            Self::Logs(args) => args.run().await,
            Self::Inspect(args) => args.run().await,
            Self::Shell(args) => args.run(),
            Self::Mounts(args) => args.run().await,
            Self::Reset(args) => args.run().await,
            Self::Completions(args) => {
                args.run();
                Ok(())
            },
            Self::Version(args) => args.run().await,
            Self::Debug(args) => args.run(),
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
