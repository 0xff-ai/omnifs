//! `omnifs frontend`: lifecycle for the optional virtualized FUSE frontend.
//!
//! There is no "room": the thing this group manages is the virtualized FUSE
//! **frontend**, a separate, credential-free guest (a Docker container or a
//! krunkit microVM, selected by `--driver`/`[frontend] driver`) attached to a
//! host-native daemon's shared namespace over its attach listener (TCP or
//! vsock). It is an opt-in attachment, not a daemon runtime mode:
//! `[system].runtime` never references it. The guest's own entrypoint runs the
//! separate `omnifs-fuse` binary (`crates/omnifs-fuse/src/bin/omnifs_fuse.rs`),
//! not this CLI; there is no hidden runner subcommand here.

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
}

impl FrontendArgs {
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        match self.command {
            FrontendCommand::Up(args) => args.run().await.map(|()| ExitCode::Success),
            FrontendCommand::Down(args) => args.run().await.map(|()| ExitCode::Success),
            FrontendCommand::Status(args) => args.run().await,
        }
    }
}
