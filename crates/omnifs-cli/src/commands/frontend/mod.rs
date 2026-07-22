//! `omnifs frontend`: lifecycle for local and guest frontend processes.
//!
//! Frontends attach to the daemon's shared namespace and contain no provider
//! runtime or credentials. Local delivery starts a sibling runner binary;
//! Docker and libkrun deliver the FUSE runner inside an isolated guest.

mod discovery;
mod lifecycle;

pub(crate) fn fs_type_parser() -> impl clap::builder::TypedValueParser<Value = omnifs_api::FsType> {
    use clap::builder::TypedValueParser as _;

    clap::builder::PossibleValuesParser::new(["fuse", "nfs"]).map(|value| match value.as_str() {
        "fuse" => omnifs_api::FsType::Fuse,
        "nfs" => omnifs_api::FsType::Nfs,
        _ => unreachable!("possible-values parser returned an unlisted filesystem"),
    })
}

pub(crate) fn frontend_runtime_parser()
-> impl clap::builder::TypedValueParser<Value = omnifs_api::FrontendRuntime> {
    use clap::builder::TypedValueParser as _;

    clap::builder::PossibleValuesParser::new(["host", "docker", "libkrun"]).map(|value| match value
        .as_str()
    {
        "host" => omnifs_api::FrontendRuntime::Host,
        "docker" => omnifs_api::FrontendRuntime::Docker,
        "libkrun" => omnifs_api::FrontendRuntime::Libkrun,
        _ => unreachable!("possible-values parser returned an unlisted runtime"),
    })
}

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
    /// Start or reconnect one supported frontend
    Enable(lifecycle::FrontendEnableArgs),
    /// Stop one instantiated frontend
    Disable(lifecycle::FrontendDisableArgs),
    /// Restart matching instantiated frontends
    Restart(lifecycle::FrontendRestartArgs),
    /// Show OS support, runtime readiness, and instantiated frontends
    Ls,
    /// Enter an instantiated Docker or libkrun frontend
    Shell(crate::commands::shell::ShellArgs),
}

impl FrontendArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            FrontendCommand::Enable(args) => args.run(output).await,
            FrontendCommand::Disable(args) => args.run(output).await,
            FrontendCommand::Restart(args) => args.run(output).await,
            FrontendCommand::Ls => discovery::run(output).await,
            FrontendCommand::Shell(args) => args.run(output).await.map(|()| ExitCode::Success),
        }
    }
}

pub(crate) use discovery::{available_frontends, default_runtime};
#[cfg(test)]
pub(crate) use lifecycle::FrontendId;
pub(crate) use lifecycle::{FrontendEnableArgs, FrontendResult, FrontendResultState};
