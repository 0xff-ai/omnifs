//! Filesystem frontends managed by the daemon.

use omnifs_api::FrontendInfo;
use omnifs_fuse::NotifierHandle;
use omnifs_fuse::mount;
use omnifs_host::registry::ProviderRegistry;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;

use crate::proc_mounts;

pub struct Frontends {
    primary: Frontend,
}

enum Frontend {
    Fuse(Fuse),
}

struct Fuse {
    mount_point: PathBuf,
    registry: Arc<ProviderRegistry>,
    notifier: NotifierHandle,
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

    pub fn mount_point(&self) -> &Path {
        match &self.primary {
            Frontend::Fuse(frontend) => &frontend.mount_point,
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
        }
    }

    pub fn invalidate_root_child(&self, name: &str) {
        match &self.primary {
            Frontend::Fuse(frontend) => {
                omnifs_fuse::invalidate_root_child(&frontend.notifier, name);
            },
        }
    }
}
