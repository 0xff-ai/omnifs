//! On-disk layout, permissions, and control-store repair.

use anyhow::Context as _;
use omnifs_core::ResourceName;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::StateStoreOptions;

pub(crate) const DATABASE_FILE: &str = "state.sqlite3";
pub(crate) const CONTROL_STORE_DIR: &str = "control-store";
pub(crate) const STAGING_DIR: &str = "staging";
pub(crate) const CACHE_DIR: &str = "cache";
pub(crate) const RUNTIME_DIR: &str = "runtime";
pub(crate) const LOG_DIR: &str = "logs";
pub(crate) const DAEMON_LOG_FILE: &str = "daemon.log";
pub(crate) const PROJECTION_CACHE_DIR: &str = "projection";
pub(crate) const WASMTIME_CACHE_DIR: &str = "wasmtime";
pub(crate) const CLONE_CACHE_DIR: &str = "git";
pub(crate) const GUEST_IMAGES_CACHE_DIR: &str = "guest-images";
pub(crate) const FILESYSTEMS_DIR: &str = "filesystems";

/// Headroom multiplier reserved against the disk budget for one import.
const PROVIDER_DISK_MULTIPLIER: u64 = 3;

static REPAIR_ARCHIVE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Paths owned by one daemon state root.
///
/// Constructing this value performs no I/O. [`Self::prepare`] creates and
/// restricts the directories needed before `SQLite` is opened, including the
/// required Wasmtime cache directory.
#[derive(Debug, Clone)]
pub struct DaemonStatePaths {
    root: PathBuf,
}

#[cfg(test)]
pub(crate) type StorePaths = DaemonStatePaths;

impl DaemonStatePaths {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn under_root(root: &Path) -> Self {
        Self::new(root)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn control_store(&self) -> PathBuf {
        self.root.join(CONTROL_STORE_DIR)
    }

    pub(crate) fn database(&self) -> PathBuf {
        self.control_store().join(DATABASE_FILE)
    }

    fn wal(&self) -> PathBuf {
        self.control_store().join(format!("{DATABASE_FILE}-wal"))
    }

    fn shm(&self) -> PathBuf {
        self.control_store().join(format!("{DATABASE_FILE}-shm"))
    }

    #[must_use]
    pub(crate) fn staging(&self) -> PathBuf {
        self.root.join(STAGING_DIR)
    }

    #[must_use]
    pub(crate) fn cache(&self) -> PathBuf {
        self.root.join(CACHE_DIR)
    }

    #[must_use]
    pub(crate) fn logs(&self) -> PathBuf {
        self.root.join(LOG_DIR)
    }

    /// Private root for daemon-owned runtime records and sockets.
    #[must_use]
    pub fn runtime(&self) -> PathBuf {
        self.root.join(RUNTIME_DIR)
    }

    /// Private directory containing one runtime subdirectory per filesystem.
    #[must_use]
    pub fn filesystems_runtime(&self) -> PathBuf {
        self.runtime().join(FILESYSTEMS_DIR)
    }

    /// Private runtime directory for one validated filesystem name.
    #[must_use]
    pub fn filesystem_runtime(&self, name: &ResourceName) -> PathBuf {
        self.filesystems_runtime().join(name.as_str())
    }

    /// Create and restrict one filesystem's private runtime directory.
    pub fn prepare_filesystem_runtime(&self, name: &ResourceName) -> anyhow::Result<PathBuf> {
        let path = self.filesystem_runtime(name);
        ensure_private_dir(&path)?;
        Ok(path)
    }

    /// Private cache for guest image layers and resolved image data.
    #[must_use]
    pub fn guest_images_cache(&self) -> PathBuf {
        self.cache().join(GUEST_IMAGES_CACHE_DIR)
    }

    /// Private directory containing one daemon-owned filesystem log per name.
    #[must_use]
    pub fn filesystem_logs(&self) -> PathBuf {
        self.logs().join(FILESYSTEMS_DIR)
    }

    /// Private log path for one validated filesystem name.
    #[must_use]
    pub fn filesystem_log(&self, name: &ResourceName) -> PathBuf {
        self.filesystem_logs()
            .join(format!("{}.log", name.as_str()))
    }

    /// Open one filesystem log with private file permissions.
    pub fn open_filesystem_log(&self, name: &ResourceName) -> anyhow::Result<std::fs::File> {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        ensure_private_dir(&self.root)?;
        ensure_private_dir(&self.logs())?;
        ensure_private_dir(&self.filesystem_logs())?;
        let path = self.filesystem_log(name);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("open filesystem log {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict filesystem log {}", path.display()))?;
        Ok(file)
    }

    pub fn prepare(&self) -> anyhow::Result<()> {
        for path in [
            self.root.clone(),
            self.control_store(),
            self.staging(),
            self.cache(),
            self.runtime(),
            self.logs(),
            self.filesystems_runtime(),
            self.filesystem_logs(),
            self.guest_images_cache(),
        ] {
            ensure_private_dir(&path)?;
        }
        let engine = self.engine_paths();
        for path in [
            engine.projection_cache(),
            engine.wasmtime_cache(),
            engine.clone_cache(),
        ] {
            ensure_private_dir(path)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn engine_paths(&self) -> crate::EngineStatePaths {
        let cache = self.cache();
        crate::EngineStatePaths {
            projection: cache.join(PROJECTION_CACHE_DIR),
            wasmtime: cache.join(WASMTIME_CACHE_DIR),
            clones: cache.join(CLONE_CACHE_DIR),
        }
    }

    pub(crate) fn restrict_database_files(&self) -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        for path in [self.database(), self.wal(), self.shm()] {
            if path.exists() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .with_context(|| format!("restrict StateStore file {}", path.display()))?;
            }
        }
        Ok(())
    }

    pub(crate) fn cleanup_staging(&self) -> anyhow::Result<()> {
        let staging = self.staging();
        for entry in std::fs::read_dir(&staging)
            .with_context(|| format!("read StateStore staging directory {}", staging.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            anyhow::ensure!(
                file_type.is_file() || file_type.is_symlink(),
                "unexpected directory entry in StateStore staging: {}",
                entry.path().display()
            );
            std::fs::remove_file(entry.path())
                .with_context(|| format!("remove stale staging file {}", entry.path().display()))?;
        }
        Ok(())
    }

    pub(crate) fn ensure_provider_disk_budget(
        &self,
        options: &StateStoreOptions,
        provider_length: u64,
    ) -> anyhow::Result<()> {
        let current = [self.database(), self.wal(), self.shm()]
            .into_iter()
            .try_fold(0_u64, |total, path| {
                let length = match std::fs::metadata(&path) {
                    Ok(metadata) => metadata.len(),
                    Err(error) if error.kind() == ErrorKind::NotFound => 0,
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("measure StateStore file {}", path.display())
                        });
                    },
                };
                total
                    .checked_add(length)
                    .context("StateStore disk use overflow")
            })?;
        let reserved = provider_length
            .checked_mul(PROVIDER_DISK_MULTIPLIER)
            .context("provider disk reservation overflow")?;
        let projected = current
            .checked_add(reserved)
            .context("StateStore disk projection overflow")?;
        anyhow::ensure!(
            projected <= options.disk_budget_bytes,
            "provider import needs up to {reserved} bytes with {current} bytes in use; \
             StateStore budget is {} bytes",
            options.disk_budget_bytes
        );
        Ok(())
    }

    /// Move the authoritative control store aside as one directory entry.
    pub(crate) fn archive_control_store(&self) -> anyhow::Result<Option<PathBuf>> {
        let source = self.control_store();
        match std::fs::symlink_metadata(&source) {
            Ok(_) => {},
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("inspect control store before repair"),
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = REPAIR_ARCHIVE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let archive = self.root.join(format!(
            "{CONTROL_STORE_DIR}.corrupt.{}.{}.{}",
            std::process::id(),
            nonce,
            sequence
        ));
        anyhow::ensure!(
            !archive.exists(),
            "control-store archive target already exists"
        );
        std::fs::rename(&source, &archive).context("archive corrupt control store")?;
        Ok(Some(archive))
    }

    pub(crate) fn rollback_control_store(&self, archive: Option<&Path>) -> anyhow::Result<()> {
        remove_control_store_entry(&self.control_store())?;
        if let Some(archive) = archive {
            std::fs::rename(archive, self.control_store())
                .context("restore archived control store")?;
        }
        Ok(())
    }
}

fn remove_control_store_entry(path: &Path) -> anyhow::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect replacement control store"),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path).context("remove replacement control store")
    } else {
        std::fs::remove_file(path).context("remove replacement control-store entry")
    }
}

pub(crate) fn ensure_private_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "StateStore path is not a directory: {}",
                path.display()
            );
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("restrict StateStore directory {}", path.display()))?;
        },
        Err(error) if error.kind() == ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .with_context(|| format!("create StateStore directory {}", path.display()))?;
        },
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect StateStore directory {}", path.display()));
        },
    }
    Ok(())
}
