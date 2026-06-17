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
}

impl DownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        use crate::paths::PathOverrides;

        let DownArgs { container_name } = self;
        let (paths, config) = crate::paths::resolve_with_config(PathOverrides::default())?;

        if config.runtime() == crate::config::Runtime::Native {
            return teardown_native(&paths.cache_dir.join("nfs"));
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

/// Tear down host-native NFS mounts recorded under `state_dir` and report what
/// actually happened (a live unmount, an orphan sweep, or nothing).
fn teardown_native(state_dir: &std::path::Path) -> anyhow::Result<()> {
    let summary = crate::host_teardown::teardown_host_native(state_dir)?;
    if summary.unmounted > 0 {
        anstream::println!("✓ Unmounted {} host-native mount(s)", summary.unmounted);
    }
    if summary.swept_orphans > 0 {
        anstream::println!(
            "✓ Swept {} orphaned mount-state file(s)",
            summary.swept_orphans
        );
    }
    for path in &summary.failed {
        anstream::eprintln!(
            "warning: {} is still mounted; re-run `omnifs down`",
            path.display()
        );
    }
    if summary.unmounted == 0 && summary.swept_orphans == 0 && summary.failed.is_empty() {
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
