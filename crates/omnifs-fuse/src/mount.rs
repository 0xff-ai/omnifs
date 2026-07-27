//! FUSE mount and unmount operations.
//!
//! Provides `run_blocking` to start the FUSE filesystem over a
//! [`Namespace`](omnifs_vfs::Namespace) and `unmount` for clean teardown via
//! fusermount.

use crate::{FuseAdapter, NotifierHandle};
use fuser::Session;
use omnifs_mtab::{Platform, UnmountCommand};
use omnifs_vfs::Namespace;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::runtime::Handle;
use tracing::info;

/// Mount the FUSE filesystem over `namespace` and block until it exits. The
/// daemon owns namespace construction and hands the filesystem a `dyn Namespace`.
/// Provider teardown is the daemon's responsibility after `serve` returns, not
/// this function's, so FUSE and NFS tear down symmetrically. `notifier` is the
/// caller's handle for kernel invalidation; it is filled once the session is up
/// and cleared on exit.
pub fn run_blocking(
    mount_point: &Path,
    namespace: Arc<dyn Namespace>,
    rt: &Handle,
    notifier: &NotifierHandle,
) -> Result<(), Error> {
    run_blocking_cancellable(
        mount_point,
        namespace,
        rt,
        notifier,
        &AtomicBool::new(false),
    )
}

/// Mount and serve until unmounted, while honoring a cancellation request that
/// may arrive during `Session::new`.
pub fn run_blocking_cancellable(
    mount_point: &Path,
    namespace: Arc<dyn Namespace>,
    rt: &Handle,
    notifier: &NotifierHandle,
    cancelled: &AtomicBool,
) -> Result<(), Error> {
    let fs = FuseAdapter::new(rt.clone(), namespace, Arc::clone(notifier));
    // Apply invalidation/growth events out of band so the kernel drops
    // huge-TTL dentries even when no op is in flight.
    fs.spawn_event_pump();
    let config = FuseAdapter::mount_config();

    info!(mount = %mount_point.display(), "starting FUSE mount");

    let session =
        Session::new(fs, mount_point, &config).map_err(|e| Error::FuseFailed(e.to_string()))?;

    // A stop can arrive while `Session::new` is inside the kernel mount call.
    // Observe it before handing the session to the blocking join so a late
    // successful mount is immediately driven through normal unmount.
    if cancelled.load(Ordering::Acquire)
        && let Err(error) = unmount(mount_point)
    {
        tracing::warn!(
            %error,
            "FUSE mount completed while cancellation won; keeping the session alive until teardown succeeds"
        );
    }

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

    // Drop the notifier after the session exits.
    notifier.lock().take();

    info!("FUSE mount exited");
    result
}

pub fn unmount(mount_point: &Path) -> Result<(), Error> {
    UnmountCommand::graceful(Platform::Linux, mount_point)
        .run()
        .map_err(|error| Error::UnmountFailed(error.to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("FUSE mount failed: {0}")]
    FuseFailed(String),
    #[error("unmount failed: {0}")]
    UnmountFailed(String),
}
