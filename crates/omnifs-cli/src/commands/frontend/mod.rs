//! `omnifs frontend`: lifecycle for local and guest frontend processes.
//!
//! Frontends attach to the daemon's shared namespace and contain no provider
//! runtime or credentials. Local delivery starts a sibling runner binary;
//! Docker and libkrun deliver the FUSE runner inside an isolated guest.

mod lifecycle;

/// Guest mount path shared by Docker and libkrun frontend runners.
pub(crate) const GUEST_MOUNT: &str = "/omnifs";

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
    Enable(lifecycle::FrontendEnableArgs),
    Disable(lifecycle::FrontendDisableArgs),
    Restart(lifecycle::FrontendRestartArgs),
    Ls(lifecycle::FrontendLsArgs),
    Shell(crate::commands::shell::ShellArgs),
}

impl FrontendArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            FrontendCommand::Enable(args) => args.run(output).await,
            FrontendCommand::Disable(args) => args.run(output).await,
            FrontendCommand::Restart(args) => args.run(output).await,
            FrontendCommand::Ls(args) => args.run(output).await,
            FrontendCommand::Shell(args) => args.run(output).await.map(|()| ExitCode::Success),
        }
    }
}

pub(crate) use lifecycle::{
    FrontendEnableArgs, FrontendFilesystem, FrontendId, FrontendResult, FrontendResultState,
    FrontendRuntime,
};
