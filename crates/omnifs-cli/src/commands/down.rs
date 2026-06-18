//! `omnifs down` — container lifecycle: stop.

use clap::Args;

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
    /// Force the Docker container path even on macOS.
    ///
    /// macOS defaults to tearing down the host-native daemon (NFS);
    /// `--isolated` selects the Docker container backend instead. On Linux the
    /// Docker path is always used and this flag has no effect.
    #[arg(long)]
    pub isolated: bool,
}

impl DownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        use crate::paths::PathOverrides;

        let DownArgs {
            container_name,
            isolated,
        } = self;
        let (paths, config) = crate::paths::resolve_with_config(PathOverrides::default())?;

        // macOS tears down the host-native NFS mount by default; `--isolated`
        // forces the Docker container path. Linux always uses Docker.
        let host_native = cfg!(target_os = "macos") && !isolated;
        if host_native {
            let summary = crate::host_teardown::teardown_host_native(&paths.nfs_state_dir())?;
            if !summary.failed.is_empty() {
                for mount_point in &summary.failed {
                    anstream::eprintln!(
                        "⚠ could not unmount {0}; unmount it manually: diskutil unmount force {0}",
                        mount_point.display()
                    );
                }
                anyhow::bail!("{} mount(s) could not be unmounted", summary.failed.len());
            }
            if summary.unmounted > 0 {
                anstream::println!("✓ omnifs unmounted");
            } else if summary.swept_orphans > 0 {
                anstream::println!(
                    "Cleaned up {} stale mount record(s); no live mount was running.",
                    summary.swept_orphans
                );
            } else if summary.skipped == 0 {
                anstream::println!("No host-native omnifs mount is running.");
            }
            // Present-but-unreadable state files (e.g. a daemon-side format bump)
            // must not be reported as "nothing running": a daemon may still hold
            // a mount we could not parse.
            if summary.skipped > 0 {
                if summary.unmounted > 0 || summary.swept_orphans > 0 {
                    anstream::eprintln!(
                        "⚠ also found {} mount-state file(s) I could not read; a daemon may still be running — try upgrading omnifs",
                        summary.skipped
                    );
                } else {
                    anyhow::bail!(
                        "{} mount-state file(s) could not be read; a daemon may still be running — try upgrading omnifs or unmount manually",
                        summary.skipped
                    );
                }
            }
            return Ok(());
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
