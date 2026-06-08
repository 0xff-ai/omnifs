//! Hidden daemon commands run inside the runtime container.

use clap::{Args, Subcommand};
use omnifs_fuse::mount;
use omnifs_host::Dirs;
use omnifs_host::cloner::GitCloner;
use omnifs_host::registry::ProviderRegistry;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::{info, warn};

#[derive(Args, Debug, Clone)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum DaemonCommand {
    /// Start the FUSE mount loop. Run inside the container by the entrypoint.
    Mount {
        #[arg(long)]
        mount_point: String,
        #[arg(long)]
        config_dir: Option<PathBuf>,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Start a direct loopback NFS mount. Run on the host for macOS/frontend tests.
    NfsMount {
        #[arg(long)]
        mount_point: PathBuf,
        #[arg(long)]
        config_dir: Option<PathBuf>,
        #[arg(long)]
        providers_dir: Option<PathBuf>,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        nfs_port: Option<u16>,
        #[arg(long)]
        nfs_trace: Option<PathBuf>,
    },
    /// Unmount a FUSE mount.
    Unmount {
        #[arg(long)]
        mount_point: PathBuf,
    },
    /// Unmount a direct loopback NFS mount.
    NfsUnmount {
        #[arg(long)]
        mount_point: PathBuf,
    },
}

impl DaemonArgs {
    pub fn run(self) -> anyhow::Result<()> {
        match self.command {
            DaemonCommand::Mount {
                mount_point,
                config_dir,
                cache_dir,
            } => mount_daemon(&mount_point, config_dir, cache_dir),
            DaemonCommand::NfsMount {
                mount_point,
                config_dir,
                providers_dir,
                cache_dir,
                state_dir,
                nfs_port,
                nfs_trace,
            } => nfs_mount_daemon(
                &mount_point,
                config_dir,
                providers_dir,
                cache_dir,
                state_dir,
                nfs_port,
                nfs_trace,
            ),
            DaemonCommand::Unmount { mount_point } => {
                mount::unmount(&mount_point)?;
                Ok(())
            },
            DaemonCommand::NfsUnmount { mount_point } => {
                omnifs_nfs::unmount(&mount_point)?;
                Ok(())
            },
        }
    }
}

fn mount_daemon(
    mount_point: &str,
    config_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    use crate::paths::{PathOverrides, Paths};

    let paths = Paths::resolve(PathOverrides {
        config_dir,
        cache_dir,
        ..Default::default()
    });
    let mount_path = PathBuf::from(&mount_point);

    std::fs::create_dir_all(&mount_path)?;
    std::fs::create_dir_all(&paths.cache_dir)?;

    info!(
        mount_point,
        config = %paths.config_dir.display(),
        cache = %paths.cache_dir.display(),
        "loading providers"
    );

    let registry = load_registry(&paths)?;
    let rt = Handle::current();
    registry.start_timers(&rt);

    let runtime_state = crate::runtime_state::RuntimeState {
        mount_point: mount_path.clone(),
        config_dir: paths.config_dir.clone(),
        cache_dir: paths.cache_dir.clone(),
        mounts_dir: paths.mounts_dir.clone(),
    };
    if let Err(error) = runtime_state.write() {
        warn!(error = %error, "failed to persist runtime state");
    }

    info!(mount_point, "starting FUSE mount");
    mount::run_blocking(&mount_path, &registry, &rt)?;
    Ok(())
}

fn nfs_mount_daemon(
    mount_point: &Path,
    config_dir: Option<PathBuf>,
    providers_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    nfs_port: Option<u16>,
    nfs_trace: Option<PathBuf>,
) -> anyhow::Result<()> {
    use crate::paths::{PathOverrides, Paths};

    let paths = Paths::resolve(PathOverrides {
        config_dir,
        cache_dir,
        providers_dir,
        ..Default::default()
    });

    std::fs::create_dir_all(mount_point)?;
    std::fs::create_dir_all(&paths.cache_dir)?;

    info!(
        mount = %mount_point.display(),
        config = %paths.config_dir.display(),
        cache = %paths.cache_dir.display(),
        "loading providers for NFS mount"
    );

    let registry = load_registry(&paths)?;
    let rt = Handle::current();
    registry.start_timers(&rt);
    let mounts = registry.mounts();
    info!(
        providers = mounts.len(),
        mounts = ?mounts,
        "loaded providers for NFS mount"
    );

    let mut options = omnifs_nfs::NfsMountOptions::loopback(
        state_dir.unwrap_or_else(|| paths.config_dir.join("nfs")),
    );
    options.bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), nfs_port.unwrap_or(0));
    options.trace_path = nfs_trace;
    options.config_dir = Some(paths.config_dir.clone());
    options.cache_dir = Some(paths.cache_dir.clone());

    info!(mount = %mount_point.display(), "starting NFS mount");
    let result = omnifs_nfs::mount_blocking(mount_point, &registry, rt, &options);
    registry.shutdown_all();
    result?;
    Ok(())
}

fn load_registry(paths: &crate::paths::Paths) -> anyhow::Result<Arc<ProviderRegistry>> {
    let cloner = Arc::new(GitCloner::new(paths.cache_dir.clone()));
    let dirs = Dirs::new(
        cloner.cache_dir(),
        &paths.config_dir,
        &paths.mounts_dir,
        &paths.providers_dir,
    );
    Ok(Arc::new(ProviderRegistry::load(dirs, &cloner)?))
}
