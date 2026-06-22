//! Daemon stop and backend reclaim workflows.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::launch_backend::LaunchBackend;
use crate::launch_record::{LaunchRecord, backend_from_daemon};
use crate::workspace::Workspace;

pub(crate) struct DaemonTeardown<'a> {
    workspace: &'a Workspace,
}

impl<'a> DaemonTeardown<'a> {
    pub(crate) fn new(workspace: &'a Workspace) -> Self {
        Self { workspace }
    }

    /// Stop the daemon and reclaim its backend. The backend is identified from
    /// the live daemon or launch record, never from `[system].runtime`.
    pub(crate) async fn down(&self, force: bool) -> anyhow::Result<()> {
        let layout = self.workspace.layout();
        let config_dir = &layout.config_dir;
        let nfs_state_dir = layout.nfs_state_dir();

        // Step 1: a live daemon answers; ask it to shut down, then reclaim.
        if let Some(status) = self.live_status_for_sweep().await? {
            anstream::println!("Stopping daemon (pid {})...", status.pid);
            match self.workspace.daemon().shutdown().await? {
                Some(report) => {
                    wait_unmounted(&report.mount_point, force)?;
                    anstream::println!("✓ Unmounted {}", report.mount_point.display());
                },
                None => {
                    // Daemon disappeared between probe and shutdown; the
                    // reclaim below sweeps whatever it left behind.
                    anstream::println!("Daemon exited before shutdown completed; sweeping...");
                },
            }
            let backend = backend_from_daemon(status.backend, config_dir)?;
            backend
                .reclaim(Some(status.mount_point.as_path()), &nfs_state_dir)
                .await?;
            LaunchRecord::remove(config_dir)?;
            return Ok(());
        }

        // Step 2: no live daemon; the launch record says what was started.
        if let Some(record) = LaunchRecord::read(config_dir)? {
            let mount_point = record.mount_point().map(Path::to_path_buf);
            anstream::println!("No live daemon found; sweeping from launch record...");
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

    /// Best-effort daemon teardown for `omnifs reset`.
    pub(crate) async fn reset_best_effort(&self) {
        let layout = self.workspace.layout();
        let config_dir = &layout.config_dir;
        let nfs_state_dir = layout.nfs_state_dir();

        let backend = match self.backend_from_live_or_record().await {
            Ok(Some(backend)) => backend,
            Ok(None) => {
                anstream::println!("⚠  No running daemon found; skipping daemon teardown");
                return;
            },
            Err(error) => {
                anstream::eprintln!("⚠  Could not identify daemon backend: {error:#}");
                return;
            },
        };

        let mount_point = match self.workspace.daemon().shutdown().await {
            Ok(Some(report)) => {
                anstream::println!("✓ Daemon stopped");
                Some(report.mount_point)
            },
            Ok(None) => {
                anstream::println!("No daemon answered shutdown; sweeping...");
                None
            },
            Err(error) => {
                anstream::eprintln!("⚠  Daemon shutdown call failed: {error:#}");
                None
            },
        };

        if let Err(error) = backend
            .reclaim(mount_point.as_deref(), &nfs_state_dir)
            .await
        {
            anstream::eprintln!("⚠  Backend reclaim failed: {error:#}");
        }

        let _ = LaunchRecord::remove(config_dir);
    }

    /// Probe the control port for teardown. Any status error is treated as
    /// "not reachable" so a sick-but-present daemon falls through to the
    /// launch-record sweep instead of failing `omnifs down` hard.
    async fn live_status_for_sweep(&self) -> anyhow::Result<Option<omnifs_api::DaemonStatus>> {
        match self.workspace.daemon().status_optional().await {
            Ok(status) => Ok(status),
            Err(_) => Ok(None),
        }
    }

    /// Identify the backend from the live daemon or the launch record.
    async fn backend_from_live_or_record(&self) -> anyhow::Result<Option<LaunchBackend>> {
        let config_dir = &self.workspace.layout().config_dir;
        if let Ok(status) = self.workspace.daemon().status().await {
            let backend = backend_from_daemon(status.backend, config_dir)?;
            return Ok(Some(backend));
        }

        if let Some(record) = LaunchRecord::read(config_dir)? {
            return Ok(Some(record.into_backend()?));
        }

        Ok(None)
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
