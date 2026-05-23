//! `omnifs logs` — tail container output.

use anyhow::Context;
use bollard::Docker;
use bollard::query_parameters::LogsOptions;
use clap::Args;
use futures_util::StreamExt;
use std::process::Command;

use crate::session::{self, ENV_CONTAINER_NAME};

#[derive(Args, Debug, Clone, Default)]
pub struct LogsArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,
    #[arg(short = 'f', long)]
    pub follow: bool,
}

impl LogsArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        use crate::paths::{PathOverrides, Paths};

        let (_paths, config) = Paths::resolve_with_config(PathOverrides::default())?;
        let container_name = self
            .container_name
            .or_else(|| session::env_string(ENV_CONTAINER_NAME))
            .or(config.container_name)
            .unwrap_or_else(|| session::CONTAINER_NAME.to_string());
        if self.follow {
            // Tail the daemon's file log; bollard's `logs` API only surfaces stdout
            // and the entrypoint writes through a `tee` pipe that buffers at EOL
            // boundaries.
            let status = Command::new("docker")
                .args(["exec", &container_name, "tail", "-F", "/tmp/omnifs.log"])
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
            &container_name,
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
