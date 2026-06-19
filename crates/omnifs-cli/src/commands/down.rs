//! `omnifs down` — daemon lifecycle: stop.

use std::path::Path;
use std::time::Duration;

use clap::Args;

use crate::client::DaemonClient;
use crate::runtime::Runtime;
use crate::runtime_target::RuntimeTarget;

#[derive(Args, Debug, Clone, Default)]
pub struct DownArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
}

impl DownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        use crate::paths::PathOverrides;

        let DownArgs { container_name } = self;
        let (paths, config) = crate::paths::resolve_with_config(PathOverrides::default())?;

        if config.runtime() == crate::config::Runtime::Native {
            return teardown_native(&paths).await;
        }

        let container_name = RuntimeTarget::resolve_container_name(container_name, &config)?;
        let remove_result = match Runtime::connect_docker() {
            Ok(runtime) => runtime.remove_existing(&container_name).await,
            Err(error) => Err(error),
        };
        remove_result?;
        anstream::println!("✓ Container `{container_name}` removed");

        // Best-effort teardown of the kubernetes dev cluster stack, if this is
        // a workspace checkout. Idempotent when no cluster is running.
        if let Ok(workspace) = crate::dev_support::WorkspaceRoot::discover()
            && let Err(error) = crate::kubernetes_testenv::down(workspace.path())
        {
            anstream::eprintln!("note: dev cluster teardown: {error:#}");
        }
        Ok(())
    }
}

/// Stop a host-native daemon. The daemon owns the frontend handle, so the CLI
/// asks it to unmount itself and waits for the mount to settle. Only a dead
/// daemon falls back to the platform sweep (stale state, stuck mount).
async fn teardown_native(paths: &crate::paths::Paths) -> anyhow::Result<()> {
    match DaemonClient::new().shutdown().await? {
        Some(report) => {
            wait_unmounted(&report.mount_point)?;
            anstream::println!("✓ Unmounted {}", report.mount_point.display());
            Ok(())
        },
        None => fallback_sweep(paths),
    }
}

/// Poll until `mount_point` leaves the OS mount table (the daemon unmounts
/// shortly after answering shutdown), up to ~3s.
fn wait_unmounted(mount_point: &Path) -> anyhow::Result<()> {
    for attempt in 0..12 {
        if !omnifs_nfs::mount_is_active(mount_point) {
            return Ok(());
        }
        if attempt + 1 < 12 {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    anyhow::bail!(
        "{} is still mounted after shutdown; re-run `omnifs down`",
        mount_point.display()
    )
}

/// Dead-daemon fallback: sweep a stale mount the daemon can no longer unmount
/// itself.
fn fallback_sweep(paths: &crate::paths::Paths) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let _ = paths;
        let mount_point = crate::paths::default_host_mount_point()?;
        if crate::host_teardown::teardown_host_native_fuse(&mount_point)? {
            anstream::println!("✓ Unmounted {}", mount_point.display());
        } else {
            anstream::println!("Nothing to tear down.");
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    teardown_native_nfs(&paths.nfs_state_dir())
}

/// Tear down host-native NFS mounts recorded under `state_dir` and report what
/// actually happened (a live unmount, an orphan sweep, or nothing).
#[cfg(not(target_os = "linux"))]
fn teardown_native_nfs(state_dir: &std::path::Path) -> anyhow::Result<()> {
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
    if !summary.failed.is_empty() {
        for path in &summary.failed {
            anstream::eprintln!("warning: {} is still mounted", path.display());
        }
        anyhow::bail!(
            "{} host-native mount(s) could not be unmounted; re-run `omnifs down`",
            summary.failed.len()
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
    Ok(())
}
