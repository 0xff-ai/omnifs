//! Filesystem frontends managed by the daemon.

use omnifs_api::FrontendInfo;
use omnifs_fuse::NotifierHandle;
use omnifs_fuse::mount;
use omnifs_host::registry::ProviderRegistry;
use omnifs_nfs::NfsMountOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;

use crate::proc_mounts;

pub struct Frontends {
    primary: Frontend,
}

enum Frontend {
    Fuse(Fuse),
    Nfs(Nfs),
}

struct Fuse {
    mount_point: PathBuf,
    registry: Arc<ProviderRegistry>,
    notifier: NotifierHandle,
}

struct Nfs {
    mount_point: PathBuf,
    registry: Arc<ProviderRegistry>,
    options: NfsMountOptions,
}

impl Frontends {
    pub fn fuse(
        mount_point: PathBuf,
        registry: Arc<ProviderRegistry>,
        notifier: NotifierHandle,
    ) -> Self {
        Self {
            primary: Frontend::Fuse(Fuse {
                mount_point,
                registry,
                notifier,
            }),
        }
    }

    pub fn nfs(
        mount_point: PathBuf,
        registry: Arc<ProviderRegistry>,
        options: NfsMountOptions,
    ) -> Self {
        Self {
            primary: Frontend::Nfs(Nfs {
                mount_point,
                registry,
                options,
            }),
        }
    }

    pub fn mount_point(&self) -> &Path {
        match &self.primary {
            Frontend::Fuse(frontend) => &frontend.mount_point,
            Frontend::Nfs(frontend) => &frontend.mount_point,
        }
    }

    pub fn serve(&self, rt: &Handle) -> anyhow::Result<()> {
        match &self.primary {
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
        match &self.primary {
            Frontend::Fuse(frontend) => proc_mounts::find_mount(&frontend.mount_point)
                .filter(|mount| mount.source == "omnifs" && mount.fs_type.starts_with("fuse"))
                .map(|mount| FrontendInfo {
                    source: mount.source,
                    fs_type: mount.fs_type,
                }),
            Frontend::Nfs(frontend) => nfs_serving(&frontend.mount_point),
        }
    }

    pub fn invalidate_root_child(&self, name: &str) {
        match &self.primary {
            Frontend::Fuse(frontend) => {
                omnifs_fuse::invalidate_root_child(&frontend.notifier, name);
            },
            Frontend::Nfs(_) => {
                let _ = name;
            },
        }
    }
}

/// NFS readiness on Linux: read the kernel mount table at `/proc/mounts`.
#[cfg(target_os = "linux")]
fn nfs_serving(mount_point: &Path) -> Option<FrontendInfo> {
    proc_mounts::find_mount(mount_point)
        .filter(|mount| mount.fs_type.starts_with("nfs"))
        .map(|mount| FrontendInfo {
            source: mount.source,
            fs_type: mount.fs_type,
        })
}

/// NFS readiness off Linux (macOS): there is no `/proc/mounts`, so read the
/// live OS mount table through omnifs-nfs. Without this, host-native `up` on
/// macOS never reports ready and `up` times out.
#[cfg(not(target_os = "linux"))]
fn nfs_serving(mount_point: &Path) -> Option<FrontendInfo> {
    omnifs_nfs::mount_is_active(mount_point).then(|| FrontendInfo {
        source: "omnifs".to_string(),
        fs_type: "nfs".to_string(),
    })
}
