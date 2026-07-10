//! Daemon stop and backend reclaim workflows.

#[cfg(feature = "daemon")]
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "daemon")]
use std::time::Duration;

use crate::launch_backend::LaunchBackend;
use crate::workspace::Workspace;
use anyhow::Context as _;
use omnifs_workspace::runtime_record::{RecordedBackend, RuntimeRecord};

pub(crate) struct DaemonTeardown<'a> {
    workspace: &'a Workspace,
}

impl<'a> DaemonTeardown<'a> {
    pub(crate) fn new(workspace: &'a Workspace) -> Self {
        Self { workspace }
    }

    /// Stop the daemon and reclaim its backend. The backend is identified from
    /// the live daemon or the runtime record, never from `[system].runtime`.
    ///
    /// Tears down a running Docker-hosted FUSE frontend first: it attaches to
    /// the daemon's namespace, so stopping the daemon out from under it would
    /// leave an orphaned, non-functional container.
    pub(crate) async fn down(&self, force: bool) -> anyhow::Result<()> {
        let layout = self.workspace.layout();
        let record_path = layout.runtime_record_file();
        let nfs_state_dir = layout.nfs_state_dir();

        self.teardown_frontend().await;

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
                backend.reclaim(Some(status.mount_point.as_path()), &nfs_state_dir)?;
                RuntimeRecord::remove(&record_path)?;
            },
            Some(RunningBackend::Cached { mount_point, pid }) => {
                anstream::println!("No live daemon found; sweeping from runtime record...");
                Self::signal_stale_native_daemon(pid);
                LaunchBackend::Native.reclaim(mount_point.as_deref(), &nfs_state_dir)?;
                RuntimeRecord::remove(&record_path)?;
            },
            None => {
                // No live daemon and no runtime record, but a wedged host-native
                // NFS mount can outlive both. On non-Linux, sweep the NFS state
                // dir so those recover; the fail-safe "unknown backend" stop
                // lives in the error paths of resolve_running_backend.
                Self::sweep_orphaned_host_native_nfs(&nfs_state_dir);
            },
        }
        Ok(())
    }

    /// A native record's pid is only trustworthy if the process is still alive.
    /// A live pid means the daemon is wedged (it did not answer the probe): send
    /// it a SIGTERM so it unmounts before the sweep. A dead pid (the common
    /// crash case) is left alone; the sweep reclaims its stranded mount.
    fn signal_stale_native_daemon(pid: u32) {
        if pid_is_alive(pid) {
            anstream::println!("Signalling wedged daemon (pid {pid}) to stop...");
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }

    /// With no live daemon and no runtime record, a wedged host-native NFS mount
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
        let record_path = layout.runtime_record_file();
        let nfs_state_dir = layout.nfs_state_dir();

        self.teardown_frontend().await;

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

        let (backend, mount_point) = match running {
            RunningBackend::Live { status, backend } => {
                let fallback_mount_point = Some(status.mount_point);
                let mount_point = match self.workspace.daemon().shutdown().await {
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
                };
                (backend, mount_point)
            },
            RunningBackend::Cached { mount_point, .. } => (LaunchBackend::Native, mount_point),
        };

        if let Err(error) = backend.reclaim(mount_point.as_deref(), &nfs_state_dir) {
            anstream::eprintln!("⚠  Backend reclaim failed: {error:#}");
        }

        let _ = RuntimeRecord::remove(&record_path);
    }

    async fn teardown_frontend(&self) {
        if let Err(error) = crate::commands::frontend::down::teardown(self.workspace.layout()).await
        {
            anstream::eprintln!("⚠  Frontend container teardown failed: {error:#}");
        }
    }

    /// Probe the control port for teardown. Any status error is treated as
    /// "not reachable" so a sick-but-present daemon falls through to the
    /// record sweep instead of failing `omnifs down` hard.
    async fn live_status_for_sweep(&self) -> Option<omnifs_api::DaemonStatus> {
        self.workspace
            .daemon()
            .status_optional()
            .await
            .ok()
            .flatten()
    }

    /// Identify the backend from the live daemon or the runtime record.
    async fn resolve_running_backend(&self) -> anyhow::Result<Option<RunningBackend>> {
        let record_path = self.workspace.layout().runtime_record_file();
        if let Some(status) = self.live_status_for_sweep().await {
            let backend = LaunchBackend::try_from(&status.backend)
                .context("unknown backend: daemon did not report reclaimable identity")?;
            return Ok(Some(RunningBackend::Live {
                status: Box::new(status),
                backend,
            }));
        }

        if let Some(record) = RuntimeRecord::read(&record_path)? {
            let mount_point = record.mount_point().map(Path::to_path_buf);
            let RecordedBackend::Native { pid } = record.backend;
            return Ok(Some(RunningBackend::Cached { mount_point, pid }));
        }

        Ok(None)
    }
}

/// True when `pid` names a live process (`kill -0` succeeds, or fails for a
/// reason other than "no such process"). Used before trusting a native record's
/// pid for a stale sweep.
fn pid_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

enum RunningBackend {
    Live {
        status: Box<omnifs_api::DaemonStatus>,
        backend: LaunchBackend,
    },
    Cached {
        mount_point: Option<PathBuf>,
        pid: u32,
    },
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
