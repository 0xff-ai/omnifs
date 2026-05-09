//! FUSE mount and unmount operations.
//!
//! Provides `mount_blocking` to start the FUSE filesystem and
//! `unmount` for clean teardown via fusermount.

use crate::fuse::FuseFs;
use crate::path_key::PathToInode;
use crate::registry::ProviderRegistry;
use dashmap::DashMap;
use fuser::{Notifier, Session};
use parking_lot::Mutex;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tokio::runtime::Handle;

/// Mount the FUSE filesystem and block until it exits. Calls
/// `registry.shutdown_all()` on exit regardless of how the mount ends.
pub fn mount_blocking(
    mount_point: &Path,
    registry: &Arc<ProviderRegistry>,
    rt: Handle,
) -> Result<(), MountError> {
    // Create shared path_to_inode map for invalidation.
    let path_to_inode: Arc<PathToInode> = Arc::new(DashMap::new());
    let notifier: Arc<Mutex<Option<Notifier>>> = Arc::new(Mutex::new(None));

    let fs = FuseFs::new_with_path_map_and_notifier(
        rt,
        Arc::clone(registry),
        Arc::clone(&path_to_inode),
        Arc::clone(&notifier),
    );
    let config = FuseFs::mount_config();

    tracing::info!(mount = %mount_point.display(), "starting FUSE mount");

    let session = Session::new(fs, mount_point, &config)
        .map_err(|e| MountError::FuseFailed(e.to_string()))?;

    // Extract the notifier before spawning the session — `spawn` takes
    // `Session` by value. The notifier only needs the message channel,
    // which is shared between foreground and background halves.
    *notifier.lock() = Some(session.notifier());
    for (mount, runtime) in registry.runtime_entries() {
        runtime.install_invalidation(Arc::clone(&path_to_inode), Arc::clone(&notifier), mount);
    }

    // fuser 0.17 removed the public `Session::run`; the supported
    // blocking pattern is to spawn onto a background thread and join
    // it. `BackgroundSession::join` returns when the FUSE loop exits,
    // so the surrounding block-until-unmount semantics are preserved.
    let background = session
        .spawn()
        .map_err(|e| MountError::FuseFailed(e.to_string()))?;
    let result = background
        .join()
        .map_err(|e| MountError::FuseFailed(e.to_string()));

    // Drop the notifier before joining the session.
    notifier.lock().take();

    tracing::info!("FUSE mount exited, shutting down providers");
    registry.shutdown_all();

    result
}

pub fn unmount(mount_point: &Path) -> Result<(), MountError> {
    let status = Command::new("fusermount")
        .args(["-u", &mount_point.display().to_string()])
        .status()
        .map_err(|e| MountError::UnmountFailed(e.to_string()))?;

    if status.success() {
        Ok(())
    } else {
        Err(MountError::UnmountFailed(format!(
            "fusermount exited with {status}"
        )))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("FUSE mount failed: {0}")]
    FuseFailed(String),
    #[error("unmount failed: {0}")]
    UnmountFailed(String),
}
