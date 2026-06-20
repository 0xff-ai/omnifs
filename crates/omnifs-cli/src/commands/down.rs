//! `omnifs down`: daemon lifecycle stop.
//!
//! Resolution order:
//!   1. Probe the control port: if a live daemon answers, trust
//!      `DaemonStatus.launch` to identify the backend.
//!   2. Fall back to the launch record: if the daemon is dead, the record
//!      says what was started.
//!   3. If neither applies, nothing is running.
//!
//! The backend is never inferred from `[system].runtime`. `down` is
//! backend-transparent: it dispatches through `Backend::reclaim` without
//! naming Docker or native.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;

use crate::client::DaemonClient;
use crate::launch_record::{LaunchRecord, backend_from_launch_kind};
use crate::paths::{PathOverrides, Paths};

#[derive(Args, Debug, Clone, Default)]
pub struct DownArgs {
    /// Force the host-native unmount if a clean shutdown leaves the mount busy.
    #[arg(long)]
    pub force: bool,
}

impl DownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let DownArgs { force } = self;
        let paths = Paths::resolve(PathOverrides::default())?;

        teardown_daemon(&paths, force).await?;

        // A dev sandbox in a workspace checkout also brings up a local
        // Kubernetes cluster (via `omnifs dev`); tear it down so `omnifs down`
        // is a full stop. A no-op outside a workspace checkout, so production
        // `down` never touches it.
        teardown_dev_cluster();
        Ok(())
    }
}

/// Stop the running daemon and reclaim its backend using the module's
/// resolution order. The backend is identified from the live daemon or the
/// launch record, never from `[system].runtime`.
async fn teardown_daemon(paths: &crate::paths::Paths, force: bool) -> anyhow::Result<()> {
    let config_dir = &paths.config_dir;
    let nfs_state_dir = paths.nfs_state_dir();
    let client = DaemonClient::new();

    // Step 1: a live daemon answers; ask it to shut down, then reclaim.
    if let Some(status) = probe_live_daemon(&client).await? {
        anstream::println!("Stopping daemon (pid {})…", status.pid);
        match client.shutdown().await? {
            Some(report) => {
                wait_unmounted(&report.mount_point, force)?;
                anstream::println!("✓ Unmounted {}", report.mount_point.display());
            },
            None => {
                // Daemon disappeared between probe and shutdown; the reclaim
                // below sweeps whatever it left behind.
                anstream::println!("Daemon exited before shutdown completed; sweeping…");
            },
        }
        let backend = backend_from_launch_kind(status.launch, config_dir)?;
        backend
            .reclaim(Some(status.mount_point.as_path()), &nfs_state_dir)
            .await?;
        LaunchRecord::remove(config_dir)?;
        return Ok(());
    }

    // Step 2: no live daemon; the launch record says what was started.
    if let Some(record) = LaunchRecord::read(config_dir)? {
        let mount_point = record.mount_point().map(Path::to_path_buf);
        anstream::println!("No live daemon found; sweeping from launch record…");
        let backend = record.into_backend()?;
        backend
            .reclaim(mount_point.as_deref(), &nfs_state_dir)
            .await?;
        LaunchRecord::remove(config_dir)?;
        return Ok(());
    }

    // Step 3: nothing is running.
    anstream::println!("Nothing to tear down.");
    Ok(())
}

/// Best-effort teardown of the contributor dev Kubernetes cluster. Only a
/// workspace checkout ever starts one (via `omnifs dev`); outside a workspace
/// this is a no-op.
fn teardown_dev_cluster() {
    let Ok(workspace) = crate::dev_support::WorkspaceRoot::discover() else {
        return;
    };
    if let Err(error) = crate::kubernetes_testenv::down(workspace.path()) {
        anstream::eprintln!("note: dev cluster teardown: {error:#}");
    }
}

/// Probe the control port. Returns `Some(status)` if a live daemon answered,
/// `None` otherwise. Any error or absence is treated as "not reachable" so a
/// sick-but-present daemon (a 5xx from the control API, or one mid-shutdown)
/// falls through to the launch-record sweep instead of failing `down` hard.
async fn probe_live_daemon(
    client: &DaemonClient,
) -> anyhow::Result<Option<omnifs_api::DaemonStatus>> {
    match client.version().await {
        Ok(Some(_)) => match client.status().await {
            Ok(status) => Ok(Some(status)),
            Err(_) => Ok(None),
        },
        Ok(None) | Err(_) => Ok(None),
    }
}

/// Poll until `mount_point` leaves the OS mount table (the daemon unmounts
/// shortly after answering shutdown), at a 100ms cadence for up to ~3s.
fn wait_unmounted(mount_point: &Path, force: bool) -> anyhow::Result<()> {
    if poll_unmounted(mount_point) {
        return Ok(());
    }
    if force {
        crate::host_teardown::force_unmount_host_native(mount_point);
        if poll_unmounted(mount_point) {
            return Ok(());
        }
    }
    Err(StillMounted::inspect(mount_point, force).into())
}

/// Poll the OS mount table at a 100ms cadence for up to ~3s. Returns true once
/// `mount_point` is no longer active.
fn poll_unmounted(mount_point: &Path) -> bool {
    for attempt in 0..30 {
        if !omnifs_nfs::mount_is_active(mount_point) {
            return true;
        }
        if attempt + 1 < 30 {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    false
}

#[derive(Debug)]
struct StillMounted {
    mount_point: PathBuf,
    forced: bool,
    open_handles: Option<String>,
}

impl StillMounted {
    fn inspect(mount_point: &Path, forced: bool) -> Self {
        Self {
            mount_point: mount_point.to_path_buf(),
            forced,
            open_handles: crate::host_teardown::open_handle_summary(mount_point),
        }
    }
}

impl fmt::Display for StillMounted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is still mounted; the daemon could not complete shutdown",
            self.mount_point.display()
        )?;
        if let Some(handles) = &self.open_handles {
            write!(f, "\n\n{handles}")?;
        }
        if self.forced {
            write!(
                f,
                "\n\nForced unmount was attempted and the mount is still active."
            )
        } else {
            write!(
                f,
                "\n\nClose those handles or `cd` them out of the mount, then re-run `omnifs down`."
            )?;
            write!(
                f,
                "\nUse `omnifs down --force` only if you intentionally want to break active handles."
            )
        }
    }
}

impl std::error::Error for StillMounted {}
