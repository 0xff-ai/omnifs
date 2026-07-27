//! Shared credential-free filesystem entrypoints.

#[cfg(target_os = "linux")]
pub mod fuse;
pub mod host_control;
mod lifecycle;
pub mod nfs;

use clap::Args;
use omnifs_core::fs;
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
    /// Stable configured filesystem name.
    #[arg(long)]
    name: fs::Id,
    /// OS filesystem protocol to serve.
    #[arg(long)]
    protocol: fs::Protocol,
    /// Runtime identity supplied by the launcher.
    #[arg(long)]
    runtime: fs::Runtime,
    /// Mount location resolved in the persisted filesystem spec.
    #[arg(long)]
    location: PathBuf,
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
    let spec = fs::Spec::new(args.name, args.protocol, args.runtime, args.location)?;
    let args = RunnerArgs {
        spec,
        state_dir: args.state_dir,
        attach: args.attach,
        port: args.port,
        host_control: args.host_control,
    };
    match args.spec.protocol() {
        #[cfg(target_os = "linux")]
        fs::Protocol::Fuse => fuse::run(args),
        #[cfg(not(target_os = "linux"))]
        fs::Protocol::Fuse => anyhow::bail!("FUSE is not supported on this platform"),
        fs::Protocol::Nfs => nfs::run(args),
    }
}

struct RunnerArgs {
    spec: fs::Spec,
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
