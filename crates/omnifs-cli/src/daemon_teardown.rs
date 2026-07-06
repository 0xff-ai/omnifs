//! Daemon stop and backend reclaim workflows.

#[cfg(feature = "daemon")]
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "daemon")]
use std::time::Duration;

use crate::launch_backend::LaunchBackend;
use crate::launch_record::LaunchRecord;
use crate::workspace::Workspace;
use anyhow::Context as _;

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

        match self.resolve_running_backend().await? {
            Some(RunningBackend::Live { status, backend }) => {
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
                backend
                    .reclaim(Some(status.mount_point.as_path()), &nfs_state_dir)
                    .await?;
                LaunchRecord::remove(config_dir)?;
                self.remove_control_token();
            },
            Some(RunningBackend::Cached {
                backend,
                mount_point,
            }) => {
                anstream::println!("No live daemon found; sweeping from launch record...");
                backend
                    .reclaim(mount_point.as_deref(), &nfs_state_dir)
                    .await?;
                LaunchRecord::remove(config_dir)?;
                self.remove_control_token();
            },
            None => {
                // No live daemon and no launch record, but a wedged host-native
                // NFS mount can outlive both. On non-Linux, sweep the NFS state
                // dir so those recover; the fail-safe "unknown backend" stop
                // lives in the error paths of resolve_running_backend.
                Self::sweep_orphaned_host_native_nfs(&nfs_state_dir);
            },
        }
        Ok(())
    }

    /// Remove the daemon control token after a reclaim so a stale token cannot
    /// outlive the daemon it authenticated. The daemon deletes it on graceful
    /// exit; teardown deletes it too for the case where the daemon died without
    /// cleaning up. A missing token is the normal case, not an error.
    fn remove_control_token(&self) {
        let path = self.workspace.layout().control_token_file();
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            anstream::eprintln!(
                "⚠  could not remove control token {}: {error}",
                path.display()
            );
        }
    }

    /// With no live daemon and no launch record, a wedged host-native NFS mount
    /// can still be orphaned. On non-Linux, sweep the NFS state dir; report only
    /// when it tore something down, otherwise print the plain nothing note.
    #[cfg(all(feature = "daemon", not(target_os = "linux")))]
    fn sweep_orphaned_host_native_nfs(nfs_state_dir: &Path) {
        match crate::host_teardown::teardown_host_native_nfs(nfs_state_dir) {
            Ok(summary) if summary.unmounted > 0 || summary.swept_orphans > 0 => {
                if summary.unmounted > 0 {
                    anstream::println!(
                        "✓ Unmounted {} orphaned host-native mount(s)",
                        summary.unmounted
                    );
                }
                if summary.swept_orphans > 0 {
                    anstream::println!(
                        "✓ Swept {} orphaned mount-state file(s)",
                        summary.swept_orphans
                    );
                }
            },
            Ok(_) => anstream::println!("Nothing to tear down."),
            Err(error) => anstream::eprintln!("⚠  Orphaned-mount sweep failed: {error:#}"),
        }
    }

    #[cfg(not(all(feature = "daemon", not(target_os = "linux"))))]
    fn sweep_orphaned_host_native_nfs(_nfs_state_dir: &Path) {
        anstream::println!("Nothing to tear down.");
    }

    /// Best-effort daemon teardown for `omnifs reset`.
    pub(crate) async fn reset_best_effort(&self) {
        let layout = self.workspace.layout();
        let config_dir = &layout.config_dir;
        let nfs_state_dir = layout.nfs_state_dir();

        let running = match self.resolve_running_backend().await {
            Ok(Some(running)) => running,
            Ok(None) => {
                anstream::println!(
                    "⚠  No running daemon or launch record; skipping daemon teardown"
                );
                return;
            },
            Err(error) => {
                anstream::eprintln!("⚠  Could not identify daemon backend: {error:#}");
                return;
            },
        };

        let (backend, fallback_mount_point, live) = running.into_parts();
        let mount_point = if live {
            match self.workspace.daemon().shutdown().await {
                Ok(Some(report)) => {
                    anstream::println!("✓ Daemon stopped");
                    Some(report.mount_point)
                },
                Ok(None) => {
                    anstream::println!("No daemon answered shutdown; sweeping...");
                    fallback_mount_point
                },
                Err(error) => {
                    anstream::eprintln!("⚠  Daemon shutdown call failed: {error:#}");
                    fallback_mount_point
                },
            }
        } else {
            fallback_mount_point
        };

        if let Err(error) = backend
            .reclaim(mount_point.as_deref(), &nfs_state_dir)
            .await
        {
            anstream::eprintln!("⚠  Backend reclaim failed: {error:#}");
        }

        let _ = LaunchRecord::remove(config_dir);
        self.remove_control_token();
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
    async fn resolve_running_backend(&self) -> anyhow::Result<Option<RunningBackend>> {
        let config_dir = &self.workspace.layout().config_dir;
        if let Some(status) = self.live_status_for_sweep().await? {
            let backend = LaunchBackend::try_from(&status.backend)
                .context("unknown backend: daemon did not report reclaimable identity")?;
            return Ok(Some(RunningBackend::Live {
                status: Box::new(status),
                backend,
            }));
        }

        if let Some(record) = LaunchRecord::read(config_dir)? {
            let mount_point = record.mount_point().map(Path::to_path_buf);
            return Ok(Some(RunningBackend::Cached {
                backend: record.into_backend()?,
                mount_point,
            }));
        }

        Ok(None)
    }
}

enum RunningBackend {
    Live {
        status: Box<omnifs_api::DaemonStatus>,
        backend: LaunchBackend,
    },
    Cached {
        backend: LaunchBackend,
        mount_point: Option<PathBuf>,
    },
}

impl RunningBackend {
    fn into_parts(self) -> (LaunchBackend, Option<PathBuf>, bool) {
        match self {
            Self::Live { status, backend } => (backend, Some(status.mount_point), true),
            Self::Cached {
                backend,
                mount_point,
            } => (backend, mount_point, false),
        }
    }
}

/// Poll until `mount_point` leaves the OS mount table (the daemon unmounts
/// shortly after answering shutdown), at a 100ms cadence for up to ~3s.
#[cfg(feature = "daemon")]
fn wait_unmounted(mount_point: &Path, force: bool) -> anyhow::Result<()> {
    let poll =
        || crate::host_teardown::poll_until_unmounted(mount_point, Duration::from_millis(100), 30);
    if poll() {
        return Ok(());
    }
    if force {
        crate::host_teardown::force_unmount_host_native(mount_point);
        if poll() {
            return Ok(());
        }
    }
    Err(StillMounted::inspect(mount_point, force).into())
}

#[cfg(not(feature = "daemon"))]
fn wait_unmounted(_mount_point: &Path, _force: bool) -> anyhow::Result<()> {
    anyhow::bail!(
        "this omnifs binary was built without host-native daemon support; \
         rerun teardown with a default omnifs build"
    )
}

#[cfg(feature = "daemon")]
#[derive(Debug)]
struct StillMounted {
    mount_point: PathBuf,
    forced: bool,
    open_handles: Option<String>,
}

#[cfg(feature = "daemon")]
impl StillMounted {
    fn inspect(mount_point: &Path, forced: bool) -> Self {
        Self {
            mount_point: mount_point.to_path_buf(),
            forced,
            open_handles: crate::host_teardown::open_handle_summary(mount_point),
        }
    }
}

#[cfg(feature = "daemon")]
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

#[cfg(feature = "daemon")]
impl std::error::Error for StillMounted {}
