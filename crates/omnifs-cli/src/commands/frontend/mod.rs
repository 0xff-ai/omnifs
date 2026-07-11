//! `omnifs frontend`: lifecycle for local and guest frontend processes.
//!
//! Frontends attach to the daemon's shared namespace and contain no provider
//! runtime or credentials. Local delivery starts a sibling runner binary;
//! Docker and krunkit deliver the FUSE runner inside an isolated guest.

pub mod down;
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
    /// Bring up a local, Docker, or krunkit frontend
    Up(up::FrontendUpArgs),
    /// Tear down a Docker or krunkit frontend
    Down(down::FrontendDownArgs),
}

impl FrontendArgs {
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        match self.command {
            FrontendCommand::Up(args) => args.run().await.map(|()| ExitCode::Success),
            FrontendCommand::Down(args) => args.run().await.map(|()| ExitCode::Success),
        }
    }
}
