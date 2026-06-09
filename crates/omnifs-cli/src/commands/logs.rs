//! `omnifs logs` — tail container output.

use clap::Args;

use crate::runtime::Runtime;
use crate::runtime_target::RuntimeTarget;

#[derive(Args, Debug, Clone, Default)]
pub struct LogsArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
    #[arg(short = 'f', long)]
    pub follow: bool,
}

impl LogsArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        use crate::paths::{PathOverrides, Paths};

        let (_paths, config) = Paths::resolve_with_config(PathOverrides::default())?;
        let target = RuntimeTarget::resolve(self.container_name, None, &config)?;
        let runtime = Runtime::connect_ready(&target, "omnifs logs").await?;
        let container_name = target.container_name().clone();

        if self.follow {
            runtime.exec_follow_log(&container_name).await
        } else {
            runtime.container_logs(&container_name, None).await
        }
    }
}
