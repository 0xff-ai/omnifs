//! `omnifs logs` — tail container output.

use clap::Args;

use crate::launch_backend::DockerTarget;
use crate::runtime::Runtime;
use crate::workspace::Workspace;

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
        let workspace = Workspace::resolve()?;
        let config = workspace.config()?;
        let target = DockerTarget::resolve(self.container_name, None, &config)?;
        let runtime = Runtime::connect_ready(&target, "omnifs logs").await?;
        let container_name = target.container_name().clone();

        if self.follow {
            runtime.exec_follow_log(&container_name).await
        } else {
            runtime.container_logs(&container_name, None).await
        }
    }
}
