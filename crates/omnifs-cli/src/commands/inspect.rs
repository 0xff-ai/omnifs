//! `omnifs inspect` — live JSONL inspector TUI.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Args;

use crate::error::{ExitCode, WithExitCode as _};
use crate::inspector::{ConnectionMode, PlainFormat, SourceKind, run_plain, run_tui};
use crate::ui::output::{Output, OutputMode};
use omnifs_workspace::Workspace;

/// The inspector's connection label for a live daemon. The daemon always runs
/// host-native and is addressed through the workspace's daemon record, so
/// there is no container identity to display here.
const LIVE_LABEL: &str = "daemon";

#[derive(Args, Debug, Clone, Default)]
#[command(
    after_help = "Examples:\n  omnifs inspect\n  omnifs inspect --plain\n  omnifs inspect --output jsonl\n  omnifs inspect --replay trace.jsonl"
)]
pub struct InspectArgs {
    /// Replay a captured JSONL file instead of attaching live.
    #[arg(long, value_name = "FILE", conflicts_with = "record")]
    pub replay: Option<PathBuf>,

    /// While live-attaching, also append the stream to this host path.
    #[arg(long, value_name = "FILE")]
    pub record: Option<PathBuf>,

    /// Print the human line stream instead of the interactive Inspector.
    #[arg(long)]
    pub plain: bool,
}

impl InspectArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<()> {
        match output.mode() {
            OutputMode::Json => {
                return Err(anyhow::anyhow!(
                    "inspect is an unbounded stream; use --output jsonl"
                ))
                .with_exit_code(ExitCode::Usage);
            },
            OutputMode::Jsonl => return self.run_plain(&output, PlainFormat::Jsonl).await,
            OutputMode::Human
                if self.plain
                    || !std::io::stdin().is_terminal()
                    || !std::io::stdout().is_terminal() =>
            {
                return self.run_plain(&output, PlainFormat::Human).await;
            },
            OutputMode::Human => {},
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
        let teaching_path = observed_teaching_path().await;

        tokio::task::spawn_blocking(move || run_tui(mode, label, source, teaching_path))
            .await
            .context("inspector TUI task")??;
        Ok(())
    }

    async fn run_plain(self, output: &Output, format: PlainFormat) -> anyhow::Result<()> {
        if let Some(path) = self.replay {
            return run_plain(SourceKind::Replay(path), output, format);
        }
        let workspace = Workspace::resolve()?;
        let client = crate::client::DaemonClient::for_workspace(&workspace);
        client.require_status().await?;
        check_record_path(self.record.as_deref())?;
        let endpoint = client.event_endpoint()?.context("daemon is not running")?;
        let record = self.record.clone();
        let output = output.clone();
        tokio::task::spawn_blocking(move || {
            run_plain(SourceKind::Socket { endpoint, record }, &output, format)
        })
        .await
        .context("inspector plain task")?
    }
}

/// Return only a path that current runtime observations say is usable.
/// Detached specs and static examples are not evidence that a path exists.
async fn observed_teaching_path() -> Option<String> {
    let workspace = Workspace::resolve().ok()?;
    let inventory = crate::inventory::Inventory::collect(&workspace)
        .await
        .ok()?;
    let filesystem = inventory.filesystems.iter().find(|filesystem| {
        filesystem.state.provides_access()
            && filesystem.spec.runtime() == omnifs_core::fs::Runtime::Host
    })?;
    let first_mount = inventory.mounts.first();
    let path = first_mount.map_or_else(
        || filesystem.spec.location().to_path_buf(),
        |mount| {
            filesystem
                .spec
                .location()
                .join(mount.root.strip_prefix("/").unwrap_or(mount.root.as_path()))
        },
    );
    Some(path.display().to_string())
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
