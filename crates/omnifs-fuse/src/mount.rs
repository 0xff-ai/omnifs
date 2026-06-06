//! FUSE mount and unmount operations.
//!
//! Provides `run_blocking` to start the FUSE filesystem and
//! `unmount` for clean teardown via fusermount.

use crate::Frontend;
use dashmap::DashMap;
use fuser::{Notifier, Session};
use omnifs_host::inspector;
use omnifs_host::path_key::PathToInode;
use omnifs_host::registry::ProviderRegistry;
use parking_lot::Mutex;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::info;

/// Mount the FUSE filesystem and block until it exits. Calls
/// `registry.shutdown_all()` on exit regardless of how the mount ends.
pub fn run_blocking(
    mount_point: &Path,
    registry: &Arc<ProviderRegistry>,
    rt: &Handle,
) -> Result<(), Error> {
    // Create shared path_to_inode map for invalidation.
    let path_to_inode: Arc<PathToInode> = Arc::new(DashMap::new());
    let notifier: Arc<Mutex<Option<Notifier>>> = Arc::new(Mutex::new(None));

    let fs = Frontend::new_with_path_map_and_notifier(
        rt.clone(),
        Arc::clone(registry),
        Arc::clone(&path_to_inode),
        Arc::clone(&notifier),
    );
    let config = Frontend::mount_config();

    info!(mount = %mount_point.display(), "starting FUSE mount");

    if let Some(sink) = inspector::init_global_from_env() {
        if let Some(path) = sink.tee_path() {
            info!(path = %path.display(), "inspector stream enabled (in-memory ring + file tee)");
        } else {
            info!("inspector stream enabled (in-memory ring only)");
        }
        // Spawn the UDS subscriber server on the runtime that drives
        // callouts. The returned JoinHandle is leaked intentionally:
        // the server should live as long as the daemon, and tokio
        // aborts the task when the runtime shuts down at process exit.
        if let Some(_handle) = sink.spawn_socket_server(rt) {
            info!("inspector socket server spawned");
        }
    }

    let session =
        Session::new(fs, mount_point, &config).map_err(|e| Error::FuseFailed(e.to_string()))?;

    // Extract the notifier before spawning the session — `spawn` takes
    // `Session` by value. The notifier only needs the message channel,
    // which is shared between foreground and background halves.
    *notifier.lock() = Some(session.notifier());

    // fuser 0.17 removed the public `Session::run`; the supported
    // blocking pattern is to spawn onto a background thread and join
    // it. `BackgroundSession::join` returns when the FUSE loop exits,
    // so the surrounding block-until-unmount semantics are preserved.
    let background = session
        .spawn()
        .map_err(|e| Error::FuseFailed(e.to_string()))?;
    let result = background
        .join()
        .map_err(|e| Error::FuseFailed(e.to_string()));

    // Drop the notifier before joining the session.
    notifier.lock().take();

    info!("FUSE mount exited, shutting down providers");
    registry.shutdown_all();

    result
}

pub fn unmount(mount_point: &Path) -> Result<(), Error> {
    let status = Command::new("fusermount")
        .args(["-u", &mount_point.display().to_string()])
        .status()
        .map_err(|e| Error::UnmountFailed(e.to_string()))?;

    if status.success() {
        Ok(())
    } else {
        Err(Error::UnmountFailed(format!(
            "fusermount exited with {status}"
        )))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("FUSE mount failed: {0}")]
    FuseFailed(String),
    #[error("unmount failed: {0}")]
    UnmountFailed(String),
}
