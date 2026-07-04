use dashmap::DashMap;
use omnifs_core::path::Path;
use omnifs_workspace::mounts::{Name as MountName, NameError as MountNameError};

pub type PathToInode = DashMap<PathKey, u64>;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PathKey {
    pub mount: MountName,
    pub path: Path,
}

impl PathKey {
    pub fn new(mount: MountName, path: Path) -> Self {
        Self { mount, path }
    }

    pub fn with_mount_str(mount: &str, path: Path) -> Result<Self, MountNameError> {
        Ok(Self::new(MountName::try_from(mount)?, path))
    }
}
