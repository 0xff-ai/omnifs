//! Shared credential-free filesystem entrypoints.

#[cfg(target_os = "linux")]
pub mod fuse;
pub mod host_control;
mod lifecycle;
pub mod nfs;

use clap::Args;
use omnifs_core::{
    FilesystemProtocol, FilesystemRuntime, FilesystemSpec, ResourceName, RuntimeInstanceId,
};
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
pub struct RunFsArgs {
    /// Desired filesystem name.
    #[arg(long)]
    name: ResourceName,
    /// OS filesystem protocol to serve.
    #[arg(long)]
    protocol: FilesystemProtocol,
    /// Runtime identity supplied by the launcher.
    #[arg(long)]
    runtime: FilesystemRuntime,
    /// Mount location resolved in the desired Filesystem spec.
    #[arg(long)]
    location: PathBuf,
    /// Docker image reference retained in the exact desired filesystem spec.
    #[arg(long)]
    docker_image: Option<String>,
    /// Libkrun guest image reference retained in the exact desired filesystem spec.
    #[arg(long)]
    libkrun_guest_image: Option<String>,
    /// Random identity of this launched runtime instance.
    #[arg(long)]
    runtime_instance: RuntimeInstanceId,
    /// Directory for local mount and runner state.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Path to the daemon's local VFS attach socket.
    #[arg(long)]
    attach: Option<PathBuf>,
    /// Loopback NFS server port. Zero asks the OS for an ephemeral port.
    #[arg(long, default_value_t = 0)]
    port: u16,
    #[command(flatten)]
    host_control: HostControlArgs,
}

pub fn run(args: RunFsArgs) -> anyhow::Result<()> {
    let spec = FilesystemSpec::new(
        args.protocol,
        args.runtime,
        args.location,
        args.docker_image,
        args.libkrun_guest_image,
    )?;
    let args = RunnerArgs {
        filesystem: args.name,
        spec,
        runtime_instance: args.runtime_instance.into_string(),
        state_dir: args.state_dir,
        attach: args.attach,
        port: args.port,
        host_control: args.host_control,
    };
    match args.spec.protocol() {
        #[cfg(target_os = "linux")]
        FilesystemProtocol::Fuse => fuse::run(args),
        #[cfg(not(target_os = "linux"))]
        FilesystemProtocol::Fuse => anyhow::bail!("FUSE is not supported on this platform"),
        FilesystemProtocol::Nfs => nfs::run(args),
    }
}

struct RunnerArgs {
    filesystem: ResourceName,
    spec: FilesystemSpec,
    runtime_instance: String,
    state_dir: Option<PathBuf>,
    attach: Option<PathBuf>,
    port: u16,
    host_control: HostControlArgs,
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
