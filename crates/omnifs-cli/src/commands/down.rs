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
        let (_paths, config) = crate::paths::resolve_with_config(PathOverrides::default())?;
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
