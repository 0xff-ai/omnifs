//! Shared credential-free frontend entrypoints.

#[cfg(target_os = "linux")]
pub mod fuse;
pub mod host_control;
mod lifecycle;
pub mod nfs;

use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct HostControlArgs {
    /// Host-only random process instance identity.
    #[arg(long, requires = "runner_control")]
    runner_instance: Option<String>,
    /// Host-only private lifecycle control socket.
    #[arg(long, requires = "runner_instance")]
    runner_control: Option<PathBuf>,
}

impl HostControlArgs {
    pub(crate) fn into_config(self) -> anyhow::Result<Option<lifecycle::RunnerControlConfig>> {
        match (self.runner_instance, self.runner_control) {
            (Some(instance_id), Some(socket)) => Ok(Some(lifecycle::RunnerControlConfig {
                instance_id,
                socket,
            })),
            (None, None) => Ok(None),
            _ => anyhow::bail!("--runner-instance and --runner-control must be supplied together"),
        }
    }
}

#[derive(Debug, Args)]
pub struct RunFrontendArgs {
    #[command(subcommand)]
    command: FrontendCommand,
}

#[derive(Debug, Subcommand)]
enum FrontendCommand {
    /// Attach to the daemon and serve a FUSE mount.
    #[cfg(target_os = "linux")]
    Fuse(fuse::Args),
    /// Attach to the daemon and serve an `NFSv4` loopback mount.
    Nfs(nfs::Args),
}

pub fn run(args: RunFrontendArgs) -> anyhow::Result<()> {
    match args.command {
        #[cfg(target_os = "linux")]
        FrontendCommand::Fuse(args) => fuse::run(args),
        FrontendCommand::Nfs(args) => nfs::run(args),
    }
}

pub(crate) fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(filter)
        .init();
}
