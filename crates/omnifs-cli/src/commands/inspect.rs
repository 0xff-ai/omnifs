//! `omnifs inspect` — live JSONL inspector TUI.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Args;

use crate::control::addr::daemon_addr;
use crate::inspector::{ConnectionMode, SourceKind, run_plain, run_tui};
use crate::launch_backend::{ContainerName, DockerTarget};
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct InspectArgs {
    /// Container name.
    ///
    /// Defaults to `OMNIFS_CONTAINER_NAME`, then configured session name, then
    /// `omnifs`.
    #[arg(long)]
    pub container_name: Option<String>,

    /// Replay a captured JSONL file instead of attaching live.
    #[arg(long, value_name = "FILE")]
    pub replay: Option<PathBuf>,

    /// While live-attaching, also append the stream to this host path.
    #[arg(long, value_name = "FILE")]
    pub record: Option<PathBuf>,

    /// Print raw JSONL instead of the ratatui canvas.
    #[arg(long)]
    pub plain: bool,
}

impl InspectArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        if self.plain {
            return self.run_plain().await;
        }

        let (mode, source, container) = if let Some(path) = self.replay.clone() {
            (
                ConnectionMode::Replay,
                SourceKind::Replay(path),
                "replay".to_string(),
            )
        } else {
            let container = self.resolve_container()?;
            check_record_path(self.record.as_deref())?;
            let addr = daemon_addr();
            let label = container.as_str().to_string();
            (
                ConnectionMode::Inspector,
                SourceKind::Socket {
                    addr,
                    record: self.record.clone(),
                },
                label,
            )
        };

        tokio::task::spawn_blocking(move || run_tui(mode, container, source))
            .await
            .context("inspector TUI task")??;
        Ok(())
    }

    async fn run_plain(self) -> anyhow::Result<()> {
        if let Some(path) = self.replay {
            return run_plain(SourceKind::Replay(path));
        }
        Workspace::resolve()?.daemon().require_compatible().await?;
        let _container = self.resolve_container()?;
        check_record_path(self.record.as_deref())?;
        let addr = daemon_addr();
        let record = self.record.clone();
        tokio::task::spawn_blocking(move || run_plain(SourceKind::Socket { addr, record }))
            .await
            .context("inspector plain task")?
    }

    fn resolve_container(&self) -> anyhow::Result<ContainerName> {
        let workspace = Workspace::resolve()?;
        let config = workspace.config()?;
        DockerTarget::resolve_container_name(self.container_name.clone(), &config)
    }
}

fn check_record_path(path: Option<&Path>) -> anyhow::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open record file `{}`", path.display()))?;
    Ok(())
}
