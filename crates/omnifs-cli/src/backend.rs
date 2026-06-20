//! Backend abstraction for daemon launch and stop.
//!
//! `LaunchParams` is the single data model for launch intent: it holds the
//! common parameters ([`Paths`], control address, mount point) and a
//! [`LaunchBackend`] variant with the backend-specific details. Native spawn builds typed
//! [`omnifs_daemon::DaemonArgs`] from the daemon crate so flag knowledge stays
//! next to the daemon argument surface.
//!
//! `LaunchBackend::reclaim` tears down the backend-specific resources after the
//! control-API shutdown has been attempted. Callers (down.rs, reset.rs) never
//! branch on native-vs-docker; they go through this interface.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use omnifs_daemon::{DaemonArgs, NativeLaunchConfig};
use omnifs_home::Paths;

use crate::container_name::ContainerName;
use crate::launch_backend::LaunchBackend;

/// Backend-agnostic launch intent plus the chosen backend's specifics.
#[derive(Debug, Clone)]
pub(crate) struct LaunchParams {
    pub paths: Paths,
    pub control_addr: SocketAddr,
    /// Recorded in `launch.json` after the daemon is ready; not passed on argv
    /// (the daemon resolves mount point from `OMNIFS_MOUNT_POINT` or `$HOME/omnifs`).
    pub mount_point: Option<PathBuf>,
    pub backend: LaunchBackend,
}

impl LaunchBackend {
    /// Reclaim backend-specific resources after a graceful control-API shutdown
    /// has been attempted. For native: sweep any stale mount. For Docker: stop
    /// and remove the container.
    ///
    /// Launch is the one place a backend is chosen rather than dispatched
    /// (`omnifs up`/`dev`), so it lives in `launch.rs` (native via
    /// `launch_native`, Docker via `Runtime::launch_container`) rather than on
    /// this enum. Teardown, by contrast, must be backend-transparent, which is
    /// what `reclaim` provides.
    ///
    /// `mount_point` is the mount to sweep if the daemon is already dead and
    /// left a stale mount behind. `nfs_state_dir` is where the non-Linux daemon
    /// records its mount-state files (derived from the caller's resolved paths,
    /// so it honors `OMNIFS_HOME`/cache overrides). The unmount is always forced,
    /// since reclaim runs only after the daemon stopped managing its own mount.
    pub(crate) async fn reclaim(
        &self,
        mount_point: Option<&Path>,
        nfs_state_dir: &Path,
    ) -> Result<()> {
        match self {
            LaunchBackend::Native => reclaim_native(mount_point, nfs_state_dir),
            LaunchBackend::Docker(target) => reclaim_docker(target.container_name()).await,
        }
    }
}

// --- Native launch -----------------------------------------------------------

pub(crate) async fn launch_native(params: &LaunchParams) -> Result<()> {
    use std::process::Stdio;

    use tokio::process::Command;

    use crate::client::DaemonClient;

    std::fs::create_dir_all(&params.paths.cache_dir)
        .with_context(|| format!("create cache dir {}", params.paths.cache_dir.display()))?;

    let binary = std::env::current_exe().context("resolve the omnifs executable")?;
    let log_path = params.paths.cache_dir.join("daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open daemon log {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("clone daemon log handle {}", log_path.display()))?;

    let daemon_args = DaemonArgs::from(NativeLaunchConfig {
        paths: params.paths.clone(),
        listen: params.control_addr,
    });
    let argv = daemon_args.to_argv();
    let mut command = Command::new(&binary);
    for arg in &argv {
        command.arg(arg);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    // Default the daemon to info-level logging when the user has not set
    // RUST_LOG. The CLI's own tracing defaults to warn, which would hide
    // the daemon's startup diagnostics in daemon.log.
    if std::env::var_os("RUST_LOG").is_none() {
        command.env("RUST_LOG", "info");
    }

    // Own process group so the daemon is not signalled when the CLI or its
    // shell exits.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn omnifs daemon ({})", binary.display()))?;

    // Poll readiness at a 100ms cadence (snappy startup) for up to 30s; fail
    // fast if the child exits first.
    let child_pid = child.id();
    let client = DaemonClient::new();
    for _ in 0..300 {
        if let Some(status) = child.try_wait().context("poll daemon child status")? {
            let tail = read_log_tail(&log_path);
            anyhow::bail!("omnifs daemon exited before the mount became ready ({status})\n{tail}");
        }
        if client.ready().await {
            if let Some(pid) = child_pid {
                if let Ok(status) = client.status().await {
                    if status.pid == pid {
                        // Confirmed our daemon; drop the handle (kill_on_drop
                        // is false) to detach it.
                        drop(child);
                        return Ok(());
                    }
                    let tail = read_log_tail(&log_path);
                    let _ = child.kill().await;
                    anyhow::bail!(
                        "daemon readiness came from pid {}, not spawned pid {pid}; \
                         another omnifs daemon is already serving on the control port\n{tail}",
                        status.pid
                    );
                }
            } else {
                drop(child);
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let tail = read_log_tail(&log_path);
    let _ = child.kill().await;
    anyhow::bail!("omnifs daemon did not become ready within 30s\n{tail}")
}

fn read_log_tail(log_path: &Path) -> String {
    const TAIL: usize = 4096;
    match std::fs::read(log_path) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(TAIL);
            format!(
                "--- {} (tail) ---\n{}",
                log_path.display(),
                String::from_utf8_lossy(&bytes[start..])
            )
        },
        Err(error) => format!("(could not read {}: {error})", log_path.display()),
    }
}

// --- Native reclaim ----------------------------------------------------------

/// Sweep any stale mount left by a dead host-native daemon. On Linux the FUSE
/// mount at `mount_point` is unmounted directly; on other platforms the NFS
/// mount-state files under `nfs_state_dir` drive the sweep.
pub(crate) fn reclaim_native(mount_point: Option<&Path>, nfs_state_dir: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let _ = nfs_state_dir;
        let Some(mp) = mount_point else {
            anstream::println!("Nothing to tear down.");
            return Ok(());
        };
        if crate::host_teardown::teardown_host_native_fuse(mp)? {
            anstream::println!("✓ Unmounted {}", mp.display());
        } else {
            anstream::println!("Nothing to tear down.");
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    {
        // The mount point is host-visible; the NFS server's mount-state files
        // (pid, mount point, version) live under `nfs_state_dir` and are what
        // drive an actual unmount. The caller derives `nfs_state_dir` from its
        // resolved paths, so it honors OMNIFS_HOME and cache-dir overrides.
        let _ = mount_point;
        sweep_nfs_state_dir(nfs_state_dir)
    }
}

#[cfg(not(target_os = "linux"))]
fn sweep_nfs_state_dir(state_dir: &Path) -> Result<()> {
    let summary = crate::host_teardown::teardown_host_native_nfs(state_dir)?;
    if summary.unmounted > 0 {
        anstream::println!("✓ Unmounted {} host-native mount(s)", summary.unmounted);
    }
    if summary.swept_orphans > 0 {
        anstream::println!(
            "✓ Swept {} orphaned mount-state file(s)",
            summary.swept_orphans
        );
    }
    if summary.unmounted == 0 && summary.swept_orphans == 0 {
        if summary.skipped > 0 {
            anstream::println!(
                "No teardown performed; {} mount-state file(s) were unreadable (see warnings above).",
                summary.skipped
            );
        } else {
            anstream::println!("Nothing to tear down.");
        }
    }
    if !summary.failed.is_empty() {
        anyhow::bail!("{} mount(s) could not be unmounted", summary.failed.len());
    }
    Ok(())
}

// --- Docker reclaim ----------------------------------------------------------

async fn reclaim_docker(container_name: &ContainerName) -> Result<()> {
    match crate::runtime::Runtime::connect_docker() {
        Ok(runtime) => {
            runtime.remove_existing(container_name).await?;
            anstream::println!("✓ Container `{container_name}` removed");
            Ok(())
        },
        Err(error) => {
            anstream::eprintln!(
                "⚠  Docker not reachable; could not remove container `{container_name}`: {error}"
            );
            Ok(())
        },
    }
}
