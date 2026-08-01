//! `omnifs inspect` — live JSONL inspector TUI.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use clap::Args;

use crate::error::{ExitCode, WithExitCode as _};
use crate::ui::output::{Output, OutputMode};
use omnifs_bootstrap::Profile;
use omnifs_inspector::{NonInteractiveFormat, SessionReceipt, SourceKind, run_plain, run_tui};

/// The inspector's connection label for a live daemon. The daemon always runs
/// host-native and is addressed through the profile's fixed control endpoint,
/// so there is no container identity to display here.
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
            OutputMode::Jsonl => return self.run_plain(&output, NonInteractiveFormat::Jsonl).await,
            OutputMode::Human
                if self.plain
                    || !std::io::stdin().is_terminal()
                    || !std::io::stdout().is_terminal() =>
            {
                return self.run_plain(&output, NonInteractiveFormat::Human).await;
            },
            OutputMode::Human => {},
        }

        let (source, label) = if let Some(path) = self.replay.clone() {
            (SourceKind::Replay(path), "replay".to_string())
        } else {
            let rpc = crate::rpc::RpcClient::resolve()?;
            // Probe readiness before entering the TUI so a down daemon exits 3
            // (DaemonUnavailable) the same as the `--plain` path, instead of
            // opening an empty canvas and exiting 0.
            rpc.ready().await?;
            check_record_path(self.record.as_deref())?;
            let endpoint = Profile::resolve()?.control_socket();
            (
                SourceKind::Socket {
                    endpoint,
                    record: self.record.clone(),
                },
                LIVE_LABEL.to_string(),
            )
        };
        let teaching_path = observed_teaching_path().await;

        let (receipt, run_result) =
            tokio::task::spawn_blocking(move || run_tui(label, source, teaching_path))
                .await
                .context("inspector TUI task")??;
        print_session_receipt(&output, &receipt);
        run_result?;
        Ok(())
    }

    async fn run_plain(self, output: &Output, format: NonInteractiveFormat) -> anyhow::Result<()> {
        // `on_line` prints unconditionally through the raw writer rather than
        // `Output::report` (which is a no-op outside Human mode): this
        // stream's own mode dispatch above already decided whether `format`
        // is human-plain or the canonical Jsonl passthrough, and both are
        // the actual stdout product for their mode, not something
        // `emit_result` ever wraps.
        if let Some(path) = self.replay {
            return run_plain(
                SourceKind::Replay(path),
                format,
                |message| output.narrate(message),
                crate::ui::print_raw,
            );
        }
        let rpc = crate::rpc::RpcClient::resolve()?;
        rpc.ready().await?;
        check_record_path(self.record.as_deref())?;
        let endpoint = Profile::resolve()?.control_socket();
        let record = self.record.clone();
        let output = output.clone();
        tokio::task::spawn_blocking(move || {
            run_plain(
                SourceKind::Socket { endpoint, record },
                format,
                |message| output.narrate(message),
                crate::ui::print_raw,
            )
        })
        .await
        .context("inspector plain task")?
    }
}

/// Return only a path that current runtime state says is usable.
/// Detached specs and static examples are not evidence that a path exists.
async fn observed_teaching_path() -> Option<String> {
    let inventory = crate::inventory::Inventory::collect_rpc().await.ok()?;
    let location = inventory.primary_host_location()?;
    let path = inventory.mounts.first().map_or_else(
        || location.to_path_buf(),
        |mount| location.join(mount.root.strip_prefix("/").unwrap_or(mount.root.as_path())),
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

/// Print the post-quit session receipt: duration, events, errors, cache
/// ratio, the slowest operation seen, and (when `--record` was set) where
/// the raw stream was captured. `omnifs-inspector` hands back plain typed
/// data ([`SessionReceipt`]); this crate owns turning it into flat ledger
/// rows via `ui/render.rs`, consistent with the v2 register.
fn print_session_receipt(output: &Output, receipt: &SessionReceipt) {
    output.report(render_receipt(
        receipt,
        crate::ui::render::stdout_capabilities(),
    ));
}

fn render_receipt(receipt: &SessionReceipt, caps: crate::ui::render::Capabilities) -> String {
    use crate::ui::render::{LedgerRow, ledger_block, sentence};
    use crate::ui::style::Glyph;

    let mut rows = vec![
        LedgerRow::new(Glyph::Done, "duration", format_duration(receipt.duration)),
        LedgerRow::new(Glyph::Done, "events", receipt.events.to_string()),
    ];

    rows.push(if receipt.errors == 0 {
        LedgerRow::new(Glyph::Skip, "errors", "0")
    } else {
        LedgerRow::new(Glyph::Done, "errors", receipt.errors.to_string())
    });

    rows.push(match receipt.cache_hit_ratio {
        Some(ratio) => LedgerRow::new(Glyph::Done, "cache", format!("{:.0}%", ratio * 100.0)),
        None => LedgerRow::new(Glyph::Skip, "cache", "n/a"),
    });

    rows.push(match &receipt.slowest {
        Some(op) => LedgerRow::new(
            Glyph::Done,
            "slowest",
            format!(
                "{} {} ({}, {})",
                op.mount,
                op.path,
                op.op,
                omnifs_inspector::format_latency_us(op.elapsed_us)
            ),
        ),
        None => LedgerRow::new(Glyph::Skip, "slowest", "none"),
    });

    if let Some(path) = &receipt.record_path {
        rows.push(LedgerRow::new(
            Glyph::Done,
            "recorded",
            path.display().to_string(),
        ));
    }

    format!(
        "{}\n\n{}\n",
        sentence("Session summary.", caps),
        ledger_block(&rows, caps)
    )
}

/// `1h 2m 3s`, dropping leading zero units so a short session doesn't read
/// `0h 0m 12s`.
fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, remainder) = (total / 3600, total % 3600);
    let (minutes, seconds) = (remainder / 60, remainder % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_inspector::SlowOp;

    #[test]
    fn receipt_renders_duration_events_errors_cache_and_slowest() {
        let receipt = SessionReceipt {
            duration: Duration::from_secs(75),
            events: 42,
            errors: 3,
            cache_hit_ratio: Some(0.8),
            slowest: Some(SlowOp {
                mount: "github".into(),
                path: "/raulk/omnifs".into(),
                op: "lookup".into(),
                elapsed_us: 250_000,
            }),
            record_path: Some(PathBuf::from("/tmp/inspect.jsonl")),
        };
        let caps = crate::ui::render::Capabilities {
            width: 120,
            color: false,
        };
        let rendered = render_receipt(&receipt, caps);

        assert!(rendered.starts_with("Session summary."), "{rendered:?}");
        assert!(rendered.contains("✓ duration"), "{rendered:?}");
        assert!(rendered.contains("1m 15s"), "{rendered:?}");
        assert!(rendered.contains("✓ events"), "{rendered:?}");
        assert!(rendered.contains("42"), "{rendered:?}");
        assert!(rendered.contains("✓ errors"), "{rendered:?}");
        assert!(rendered.contains('3'), "{rendered:?}");
        assert!(rendered.contains("✓ cache"), "{rendered:?}");
        assert!(rendered.contains("80%"), "{rendered:?}");
        assert!(rendered.contains("✓ slowest"), "{rendered:?}");
        assert!(rendered.contains("github"), "{rendered:?}");
        assert!(rendered.contains("lookup"), "{rendered:?}");
        assert!(rendered.contains("✓ recorded"), "{rendered:?}");
        assert!(rendered.contains("/tmp/inspect.jsonl"), "{rendered:?}");
    }

    #[test]
    fn receipt_uses_skip_glyph_and_omits_the_record_row_when_nothing_happened() {
        let receipt = SessionReceipt {
            duration: Duration::from_secs(5),
            events: 0,
            errors: 0,
            cache_hit_ratio: None,
            slowest: None,
            record_path: None,
        };
        let caps = crate::ui::render::Capabilities {
            width: 120,
            color: false,
        };
        let rendered = render_receipt(&receipt, caps);

        assert!(rendered.contains("• errors"), "{rendered:?}");
        assert!(rendered.contains("• cache"), "{rendered:?}");
        assert!(rendered.contains("n/a"), "{rendered:?}");
        assert!(rendered.contains("• slowest"), "{rendered:?}");
        assert!(rendered.contains("none"), "{rendered:?}");
        assert!(!rendered.contains("recorded"), "{rendered:?}");
    }
}
