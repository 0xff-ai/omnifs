//! `omnifs frontend`: lifecycle for the optional virtualized FUSE frontend,
//! plus the hidden out-of-process runner it launches inside the guest.
//!
//! There is no "room": the thing this group manages is the virtualized FUSE
//! **frontend**, a separate, credential-free guest (a Docker container or a
//! krunkit microVM, selected by `--driver`/`[frontend] driver`) attached to a
//! host-native daemon's shared namespace over its attach listener (TCP or
//! vsock). It is an opt-in attachment, not a daemon runtime mode:
//! `[system].runtime` never references it.

pub mod down;
pub mod status;
pub mod up;

use clap::Subcommand;

use crate::error::ExitCode;

#[derive(clap::Args, Debug)]
pub struct FrontendArgs {
    #[command(subcommand)]
    pub command: FrontendCommand,
}

#[derive(Subcommand, Debug)]
pub enum FrontendCommand {
    /// Bring up the virtualized FUSE frontend
    Up(up::FrontendUpArgs),
    /// Tear down the virtualized FUSE frontend
    Down(down::FrontendDownArgs),
    /// Report the virtualized FUSE frontend's state and attach health
    Status(status::FrontendStatusArgs),
    /// Attach a wire-backed namespace and run a renderer over it.
    ///
    /// Internal: this is what the frontend container's entrypoint runs; it is
    /// not invoked directly on the host.
    #[command(hide = true)]
    #[cfg(feature = "daemon")]
    Run(omnifs_daemon::FrontendArgs),
}

impl FrontendArgs {
    /// `None` for the internal `run` subcommand (mirrors the hidden `daemon`
    /// subcommand, never counted as CLI usage); `Some("frontend")` for the
    /// user-facing lifecycle verbs.
    pub(crate) fn telemetry_label(&self) -> Option<&'static str> {
        match &self.command {
            #[cfg(feature = "daemon")]
            FrontendCommand::Run(_) => None,
            FrontendCommand::Up(_) | FrontendCommand::Down(_) | FrontendCommand::Status(_) => {
                Some("frontend")
            },
        }
    }

    pub async fn run(self) -> anyhow::Result<ExitCode> {
        match self.command {
            FrontendCommand::Up(args) => args.run().await.map(|()| ExitCode::Success),
            FrontendCommand::Down(args) => args.run().await.map(|()| ExitCode::Success),
            FrontendCommand::Status(args) => args.run().await,
            #[cfg(feature = "daemon")]
            FrontendCommand::Run(args) => {
                omnifs_daemon::run_frontend(args).map(|()| ExitCode::Success)
            },
        }
    }
}
