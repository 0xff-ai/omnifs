//! `omnifs frontend`: lifecycle for local and guest frontend processes.
//!
//! Frontends attach to the daemon's shared namespace and contain no provider
//! runtime or credentials. Local delivery starts a sibling runner binary;
//! Docker and krunkit deliver the FUSE runner inside an isolated guest.

mod controller;

use clap::Subcommand;

use crate::error::ExitCode;
use crate::ui::output::Output;

#[derive(clap::Args, Debug)]
pub struct FrontendArgs {
    #[command(subcommand)]
    pub command: FrontendCommand,
}

#[derive(Subcommand, Debug)]
pub enum FrontendCommand {
    Enable(controller::FrontendEnableArgs),
    Disable(controller::FrontendDisableArgs),
    Restart(controller::FrontendRestartArgs),
    Ls(controller::FrontendLsArgs),
}

impl FrontendArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            FrontendCommand::Enable(args) => args.run(output).await,
            FrontendCommand::Disable(args) => args.run(output).await,
            FrontendCommand::Restart(args) => args.run(output).await,
            FrontendCommand::Ls(args) => args.run(output).await,
        }
    }
}

pub(crate) use controller::{FrontendController, FrontendResult, RuntimeState, teardown_all};
