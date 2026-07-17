//! `omnifs inspect` — live JSONL inspector TUI.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Args;

use crate::inspector::{ConnectionMode, SourceKind, run_plain, run_tui};
use crate::ui::output::Output;
use omnifs_workspace::Workspace;

/// The inspector's connection label for a live daemon. The daemon always runs
/// host-native and is addressed through the workspace's daemon record, so
/// there is no container identity to display here.
const LIVE_LABEL: &str = "daemon";

#[derive(Args, Debug, Clone, Default)]
pub struct InspectArgs {
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
    pub async fn run(self, output: Output) -> anyhow::Result<()> {
        if output.is_structured() {
            anyhow::bail!("inspect is a passthrough command and only supports human output")
        }
        if self.plain {
            return self.run_plain(&output).await;
        }

        let (mode, source, label) = if let Some(path) = self.replay.clone() {
            (
                ConnectionMode::Replay,
                SourceKind::Replay(path),
                "replay".to_string(),
            )
        } else {
            let workspace = Workspace::resolve()?;
            // Probe readiness before entering the TUI so a down daemon exits 3
            // (DaemonUnavailable) the same as the `--plain` path, instead of
            // opening an empty canvas and exiting 0.
            let client = crate::client::DaemonClient::for_workspace(&workspace);
            client.require_status().await?;
            check_record_path(self.record.as_deref())?;
            let endpoint = client.event_endpoint()?.context("daemon is not running")?;
            (
                ConnectionMode::Inspector,
                SourceKind::Socket {
                    endpoint,
                    record: self.record.clone(),
                },
                LIVE_LABEL.to_string(),
            )
        };

        tokio::task::spawn_blocking(move || run_tui(mode, label, source))
            .await
            .context("inspector TUI task")??;
        Ok(())
    }

    async fn run_plain(self, output: &Output) -> anyhow::Result<()> {
        if let Some(path) = self.replay {
            return run_plain(SourceKind::Replay(path), output);
        }
        let workspace = Workspace::resolve()?;
        let client = crate::client::DaemonClient::for_workspace(&workspace);
        client.require_status().await?;
        check_record_path(self.record.as_deref())?;
        let endpoint = client.event_endpoint()?.context("daemon is not running")?;
        let record = self.record.clone();
        let output = output.clone();
        tokio::task::spawn_blocking(move || {
            run_plain(SourceKind::Socket { endpoint, record }, &output)
        })
        .await
        .context("inspector plain task")?
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
