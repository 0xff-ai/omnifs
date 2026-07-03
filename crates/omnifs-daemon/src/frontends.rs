//! Filesystem frontends managed by the daemon.

use omnifs_api::FrontendInfo;
use omnifs_engine::MountRuntimes;
#[cfg(target_os = "linux")]
use omnifs_fuse::NotifierHandle;
#[cfg(target_os = "linux")]
use omnifs_fuse::mount;
use omnifs_nfs::NfsMountOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;

use crate::app::FrontendKind;
use crate::context::DaemonContext;
#[cfg(target_os = "linux")]
use crate::proc_mounts;

pub(crate) enum Frontend {
    #[cfg(target_os = "linux")]
    Fuse(Fuse),
    Nfs(Nfs),
}

#[cfg(target_os = "linux")]
pub(crate) struct Fuse {
    mount_point: PathBuf,
    registry: Arc<MountRuntimes>,
    notifier: NotifierHandle,
}

pub(crate) struct Nfs {
    mount_point: PathBuf,
    registry: Arc<MountRuntimes>,
    options: NfsMountOptions,
}

impl Frontend {
    pub(crate) fn from_context(context: &DaemonContext, registry: Arc<MountRuntimes>) -> Self {
        match context.frontend() {
            #[cfg(target_os = "linux")]
            FrontendKind::Fuse => Self::fuse(
                context.mount_point().to_path_buf(),
                registry,
                omnifs_fuse::new_notifier_handle(),
            ),
            #[cfg(not(target_os = "linux"))]
            FrontendKind::Fuse => {
                unreachable!("DaemonContext resolves the NFS frontend on non-Linux hosts")
            },
            FrontendKind::Nfs => Self::nfs(
                context.mount_point().to_path_buf(),
                registry,
                context.nfs_mount_options(),
            ),
        }
    }

    #[cfg(target_os = "linux")]
    fn fuse(mount_point: PathBuf, registry: Arc<MountRuntimes>, notifier: NotifierHandle) -> Self {
        Self::Fuse(Fuse {
            mount_point,
            registry,
            notifier,
        })
    }

    fn nfs(mount_point: PathBuf, registry: Arc<MountRuntimes>, options: NfsMountOptions) -> Self {
        Self::Nfs(Nfs {
            mount_point,
            registry,
            options,
        })
    }

    pub fn serve(&self, rt: &Handle) -> anyhow::Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            Frontend::Fuse(frontend) => {
                mount::run_blocking(
                    &frontend.mount_point,
                    &frontend.registry,
                    rt,
                    &frontend.notifier,
                )?;
            },
            Frontend::Nfs(frontend) => {
                omnifs_nfs::mount_blocking(
                    &frontend.mount_point,
                    &frontend.registry,
                    rt.clone(),
                    &frontend.options,
                )?;
            },
        }
        Ok(())
    }

    pub fn serving(&self) -> Option<FrontendInfo> {
        match self {
            #[cfg(target_os = "linux")]
            Frontend::Fuse(frontend) => proc_mounts::find_mount(&frontend.mount_point)
                .filter(|mount| mount.source == "omnifs" && mount.fs_type.starts_with("fuse"))
                .map(|mount| FrontendInfo {
                    source: mount.source,
                    fs_type: mount.fs_type,
                }),
            Frontend::Nfs(frontend) => nfs_serving(&frontend.mount_point),
        }
    }

    /// Unmount the serving frontend from within the daemon, which unblocks the
    /// `serve` loop so the process can shut down. Best-effort: a failure is
    /// logged, since `omnifs down` falls back to an external sweep.
    pub fn unmount(&self) {
        let result = match self {
            #[cfg(target_os = "linux")]
            Frontend::Fuse(frontend) => {
                omnifs_fuse::mount::unmount(&frontend.mount_point).map_err(|e| e.to_string())
            },
            Frontend::Nfs(frontend) => {
                omnifs_nfs::unmount(&frontend.mount_point).map_err(|e| e.to_string())
            },
        };
        if let Err(error) = result {
            tracing::warn!(%error, "self-unmount failed");
        }
    }

    pub fn invalidate_root_child(&self, name: &str) {
        match self {
            #[cfg(target_os = "linux")]
            Frontend::Fuse(frontend) => {
                omnifs_fuse::invalidate_root_child(&frontend.notifier, name);
            },
            Frontend::Nfs(_) => {
                let _ = name;
            },
        }
    }
}

#[cfg(target_os = "linux")]
fn nfs_serving(mount_point: &Path) -> Option<FrontendInfo> {
    proc_mounts::find_mount(mount_point)
        .filter(|mount| mount.fs_type.starts_with("nfs"))
        .map(|mount| FrontendInfo {
            source: mount.source,
            fs_type: mount.fs_type,
        })
}

// macOS (and any host without `/proc/mounts`) reads the live OS mount table
// through omnifs-nfs, so host-native NFS readiness works off Linux.
#[cfg(not(target_os = "linux"))]
fn nfs_serving(mount_point: &Path) -> Option<FrontendInfo> {
    omnifs_nfs::mount_is_active(mount_point).then(|| FrontendInfo {
        source: "omnifs".to_string(),
        fs_type: "nfs".to_string(),
    })
}
