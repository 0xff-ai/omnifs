//! CLI type definitions: top-level parser and command enum.

use clap::{Parser, Subcommand};

use crate::commands;

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

    /// Daemon verbs. Internal; users should run `omnifs up` instead.
    #[command(hide = true)]
    Daemon(commands::daemon::DaemonArgs),

    /// Debug utilities. Hidden from `--help`.
    #[command(hide = true)]
    Debug(commands::debug::DebugArgs),
}

use crate::outcome::CommandOutcome;

impl Commands {
    pub async fn run(self) -> anyhow::Result<CommandOutcome> {
        match self {
            Self::Status(args) => args.run().await.map(|()| CommandOutcome::Success),
            Self::Auth(args) => args.run().await.map(|()| CommandOutcome::Success),
            Self::Setup(args) => args.run().await.map(|()| CommandOutcome::Success),
            Self::Init(args) => args.run().await.map(|()| CommandOutcome::Success),
            Self::Up(args) => args.run().await.map(|()| CommandOutcome::Success),
            Self::Dev(args) => args.run().await.map(|()| CommandOutcome::Success),
            Self::Down(args) => args.run().await.map(|()| CommandOutcome::Success),
            Self::Logs(args) => args.run().await.map(|()| CommandOutcome::Success),
            Self::Shell(args) => {
                args.run()?;
                Ok(CommandOutcome::Success)
            },
            Self::Mounts(args) => {
                args.run()?;
                Ok(CommandOutcome::Success)
            },
            Self::Reset(args) => args.run().await.map(|()| CommandOutcome::Success),
            Self::Doctor(args) => args.run().await,
            Self::Completions(args) => {
                args.run();
                Ok(CommandOutcome::Success)
            },
            Self::Version(args) => args.run().await.map(|()| CommandOutcome::Success),
            Self::Daemon(args) => {
                args.run()?;
                Ok(CommandOutcome::Success)
            },
            Self::Debug(args) => {
                args.run()?;
                Ok(CommandOutcome::Success)
            },
        }
    }
}
