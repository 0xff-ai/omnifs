//! Hidden daemon commands run inside the runtime container.

use clap::{Args, Subcommand};
use omnifs_host::mount;
use omnifs_host::registry::ProviderRegistry;
use omnifs_host::runtime::RuntimeDirs;
use omnifs_host::runtime::cloner::GitCloner;
use std::path::PathBuf;
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
    /// Unmount a FUSE mount.
    Unmount {
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
            DaemonCommand::Unmount { mount_point } => {
                mount::unmount(&mount_point)?;
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

    let cloner = Arc::new(GitCloner::new(paths.cache_dir.clone()));
    let dirs = RuntimeDirs::new(
        cloner.cache_dir(),
        &paths.config_dir,
        &paths.mounts_dir,
        &paths.providers_dir,
    );

    info!(
        mount_point,
        config = %dirs.config_dir.display(),
        cache = %cloner.cache_dir().display(),
        "loading providers"
    );

    let registry = ProviderRegistry::load(dirs, &cloner)?;

    let registry = Arc::new(registry);
    let rt = Handle::current();
    registry.start_timers(&rt);

    let runtime_state = crate::runtime_state::RuntimeState {
        mount_point: mount_path.clone(),
        config_dir: dirs.config_dir.to_path_buf(),
        cache_dir: dirs.cache_dir.to_path_buf(),
        mounts_dir: dirs.mounts_dir.to_path_buf(),
    };
    if let Err(error) = runtime_state.write() {
        warn!(error = %error, "failed to persist runtime state");
    }

    info!(mount_point, "starting FUSE mount");
    mount::mount_blocking(&mount_path, &registry, rt)?;
    Ok(())
}
