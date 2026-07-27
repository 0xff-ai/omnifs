//! `omnifs inspect` — live JSONL inspector TUI.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Args;

use crate::error::{ExitCode, WithExitCode as _};
use crate::inspector::{ConnectionMode, PlainFormat, SourceKind, run_plain, run_tui};
use crate::ui::output::{Output, OutputMode};
use omnifs_workspace::{Workspace, mounts};

/// The inspector's connection label for a live daemon. The daemon always runs
/// host-native and is addressed through the workspace's daemon record, so
/// there is no container identity to display here.
const LIVE_LABEL: &str = "daemon";

/// Fallback empty-state teaching path when no local mount specs resolve to
/// one: the `dns` provider ships with every dev bundle and needs no
/// credentials, so it reads as a real path even though it's not derived
/// from this workspace's actual configuration.
const STATIC_TEACHING_PATH: &str = "~/omnifs/dns/example.com/A";

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

        // Local-only and best-effort: a replay session or a workspace with
        // no mounts yet still gets a usable empty-state hint, never a hard
        // failure, and never a daemon round trip.
        let teaching_path = Workspace::resolve()
            .ok()
            .and_then(|workspace| pick_teaching_path(&workspace))
            .unwrap_or_else(|| STATIC_TEACHING_PATH.to_string());

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

/// Pick a directory to `cat` from for the inspector's empty-state hint:
/// prefer a configured mount whose pinned provider needs no credentials (so
/// the suggested command works without an extra `omnifs auth` step first),
/// else fall back to any configured mount. Reads only local specs and the
/// provider artifact store — no daemon round trip — so it's safe to call
/// before probing daemon readiness, and cheap enough to call unconditionally.
fn pick_teaching_path(workspace: &Workspace) -> Option<String> {
    let registry = workspace.desired_state().registry().ok()?;
    let host_location = workspace
        .filesystems()
        .list()
        .ok()?
        .into_iter()
        .find(|spec| spec.runtime() == omnifs_core::fs::Runtime::Host)?
        .location()
        .to_path_buf();
    let mut fallback = None;
    for (name, spec) in registry.iter() {
        let location = host_location.join(name.as_str());
        let text = location.display().to_string();
        let no_auth = mounts::pinned_manifest(workspace.catalog(), spec)
            .ok()
            .flatten()
            .is_some_and(|manifest| manifest.auth.is_none());
        if no_auth {
            return Some(text);
        }
        fallback.get_or_insert(text);
    }
    fallback
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
