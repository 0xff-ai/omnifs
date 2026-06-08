//! `omnifs logs` — tail container output.

use anyhow::Context;
use bollard::Docker;
use bollard::query_parameters::LogsOptions;
use clap::Args;
use futures_util::StreamExt;
use std::path::PathBuf;
use std::process::Command;

use crate::app_context::AppContext;
use crate::native_runtime;
use crate::paths::PathOverrides;
use crate::runtime_mode::RuntimeMode;
use crate::runtime_target::RuntimeTarget;

#[derive(Args, Debug, Clone, Default)]
pub struct LogsArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
    /// Runtime launch mode.
    #[arg(long, value_enum)]
    pub mode: Option<RuntimeMode>,
    /// Host mount point for native mode.
    #[arg(long)]
    pub mount_point: Option<PathBuf>,
    #[arg(short = 'f', long)]
    pub follow: bool,
}

impl LogsArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let ctx = AppContext::resolve_with_runtime(
            PathOverrides::default(),
            self.container_name,
            None,
            self.mode,
            self.mount_point,
        )?;
        let paths = ctx.paths();
        let target = ctx.runtime();
        let RuntimeTarget::Docker(target) = target else {
            return native_runtime::logs(paths, self.follow);
        };
        let container_name = target.container_name();
        if self.follow {
            // Tail the daemon's file log; bollard's `logs` API only surfaces stdout
            // and the entrypoint writes through a `tee` pipe that buffers at EOL
            // boundaries.
            let status = Command::new("docker")
                .args([
                    "exec",
                    container_name.as_str(),
                    "tail",
                    "-F",
                    "/tmp/omnifs.log",
                ])
                .status()
                .context("spawn `docker exec ... tail -F`")?;
            if !status.success() {
                anyhow::bail!("docker exec exited with {status}");
            }
            return Ok(());
        }
        let docker = Docker::connect_with_local_defaults()
            .context("connect to Docker daemon (is it running?)")?;
        let mut stream = docker.logs(
            container_name.as_str(),
            Some(LogsOptions {
                stdout: true,
                stderr: true,
                timestamps: false,
                ..Default::default()
            }),
        );
        while let Some(chunk) = stream.next().await {
            let line = chunk.with_context(|| format!("read logs from `{container_name}`"))?;
            anstream::print!("{line}");
        }
        Ok(())
    }
}
