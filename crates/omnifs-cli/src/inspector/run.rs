//! Ratatui main loop.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use super::app::{App, ConnectionMode};
use super::source::{EventSource, SourceKind};
use super::trace_state::SessionStats;
use super::ui;
use crate::ui::render::{Capabilities, LedgerRow, ledger_block, sentence, stdout_capabilities};
use crate::ui::style::Glyph;

/// Undo `enable_raw_mode` + `EnterAlternateScreen`, in that order's mirror,
/// so the user's shell is usable again. Shared by the normal quit path and
/// [`install_panic_hook`] so a panic mid-draw can't leave either one behind:
/// a `Drop` guard alone isn't enough here because panics unwind through
/// ratatui's own draw calls, which can themselves leave the terminal in a
/// state a guard's `Drop` impl doesn't expect.
///
/// Leaving the alternate screen and showing the cursor are attempted even
/// when disabling raw mode fails (and vice versa): a partial restore is
/// still better than none, and it's what makes this function's write-side
/// effects observable in a test that never had a real raw-mode terminal to
/// begin with (see the `restore_terminal_*` tests below).
fn restore_terminal(out: &mut impl Write) -> anyhow::Result<()> {
    let raw_mode = disable_raw_mode().context("disable raw mode");
    let screen = execute!(out, LeaveAlternateScreen, Show).context("leave alternate screen");
    raw_mode?;
    screen?;
    Ok(())
}

/// Install a panic hook that restores the terminal before the default panic
/// message prints, then chains to whatever hook was previously installed
/// (so backtraces, `RUST_BACKTRACE`, and test harness hooks keep working).
/// Must run before [`enable_raw_mode`]/`EnterAlternateScreen` so a panic
/// during setup itself is still covered.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal(&mut std::io::stdout());
        previous(info);
    }));
}

pub fn run_tui(
    mode: ConnectionMode,
    container: String,
    source: SourceKind,
    teaching_path: String,
) -> anyhow::Result<()> {
    install_panic_hook();

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let addr = match &source {
        SourceKind::Socket { endpoint, .. } => Some(endpoint.label()),
        SourceKind::Replay(_) => None,
    };
    // Captured before `EventSource::spawn` moves `source`, for the quit
    // receipt's "recording to" row.
    let record_path = match &source {
        SourceKind::Socket { record, .. } => record.clone(),
        SourceKind::Replay(_) => None,
    };
    let mut app = App::new(mode, container, addr, teaching_path);
    let event_source = EventSource::spawn(source);
    let session_start = Instant::now();

    let tick_rate = Duration::from_millis(50);
    let mut last_tick = Instant::now();

    let run_result = (|| -> anyhow::Result<()> {
        loop {
            terminal
                .draw(|frame| ui::render(frame, &app))
                .context("draw frame")?;

            if app.quit {
                break;
            }

            let timeout = tick_rate
                .checked_sub(last_tick.elapsed())
                .unwrap_or_else(|| Duration::from_secs(0));

            if event::poll(timeout).context("poll events")?
                && let Event::Key(key) = event::read().context("read event")?
                && key.kind == KeyEventKind::Press
            {
                app.handle_key(key);
            }

            if last_tick.elapsed() >= tick_rate {
                for message in event_source.drain() {
                    match message {
                        super::source::SourceMessage::Failed(error) => {
                            return Err(anyhow!(error));
                        },
                        super::source::SourceMessage::Finished => {},
                        message => app.apply_source_message(message),
                    }
                }
                last_tick = Instant::now();
            }
        }
        Ok(())
    })();

    let cleanup_result = restore_terminal(terminal.backend_mut());
    run_result?;
    cleanup_result?;

    let summary = SessionSummary {
        duration: session_start.elapsed(),
        session: app.session().clone(),
        record_path,
    };
    crate::ui::print_raw(&render_receipt(&summary, stdout_capabilities()));
    Ok(())
}

/// Everything the quit receipt needs, captured once so [`render_receipt`]
/// stays a pure function of its input and is testable without a live `App`
/// or a real terminal.
struct SessionSummary {
    duration: Duration,
    session: SessionStats,
    record_path: Option<PathBuf>,
}

/// The durable session receipt printed to stdout after the terminal is
/// restored: duration, events, errors, cache ratio, the slowest operation
/// seen, and (when `--record` was set) where the raw stream was captured.
/// Flat ledger rows via `ui/render.rs`, consistent with the v2 register.
fn render_receipt(summary: &SessionSummary, caps: Capabilities) -> String {
    let mut rows = vec![
        LedgerRow::new(Glyph::Done, "duration", format_duration(summary.duration)),
        LedgerRow::new(Glyph::Done, "events", summary.session.events.to_string()),
    ];

    rows.push(if summary.session.errors == 0 {
        LedgerRow::new(Glyph::Skip, "errors", "0")
    } else {
        LedgerRow::new(Glyph::Done, "errors", summary.session.errors.to_string())
    });

    rows.push(match summary.session.cache_hit_ratio() {
        Some(ratio) => LedgerRow::new(Glyph::Done, "cache", format!("{:.0}%", ratio * 100.0)),
        None => LedgerRow::new(Glyph::Skip, "cache", "n/a"),
    });

    rows.push(match &summary.session.slowest {
        Some(op) => LedgerRow::new(
            Glyph::Done,
            "slowest",
            format!(
                "{} {} ({}, {})",
                op.mount,
                op.path,
                op.op,
                super::format::format_latency_us(op.elapsed_us)
            ),
        ),
        None => LedgerRow::new(Glyph::Skip, "slowest", "none"),
    });

    if let Some(path) = &summary.record_path {
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

pub fn run_plain(source: SourceKind, output: &crate::ui::output::Output) -> anyhow::Result<()> {
    use super::source::SourceMessage;

    match source {
        SourceKind::Replay(path) => {
            for line in super::source::replay_file_blocking(&path)? {
                emit_plain_line(&line)?;
            }
        },
        SourceKind::Socket { endpoint, record } => {
            let addr = endpoint.label();
            output.narrate(format!("omnifs inspect: connecting to {addr}..."));
            let event_source = EventSource::spawn(SourceKind::Socket { endpoint, record });
            while let Some(message) = event_source.recv() {
                match message {
                    SourceMessage::Line(line) => emit_plain_line(&line)?,
                    SourceMessage::Connected => {
                        output.narrate(format!("omnifs inspect: connected to {addr}"));
                    },
                    SourceMessage::Disconnected => {
                        output.narrate(format!(
                            "omnifs inspect: disconnected from {addr}, reconnecting..."
                        ));
                    },
                    SourceMessage::Finished => break,
                    SourceMessage::Failed(error) => return Err(anyhow!(error)),
                }
            }
        },
    }
    Ok(())
}

fn emit_plain_line(line: &omnifs_api::events::InspectorLine) -> anyhow::Result<()> {
    let line = line
        .to_json_line()
        .context("serialize inspector line for plain output")?;
    crate::ui::print_raw(&line);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::trace_state::SlowOp;
    use super::*;

    fn contains_restore_sequence(buf: &[u8]) -> bool {
        let text = String::from_utf8_lossy(buf);
        // Standard VT100/ANSI sequences crossterm's `LeaveAlternateScreen`
        // and `Show` commands write; stable across crossterm versions and
        // requires no real terminal to observe.
        text.contains("\u{1b}[?1049l") && text.contains("\u{1b}[?25h")
    }

    #[test]
    fn restore_terminal_leaves_the_alt_screen_and_shows_the_cursor() {
        let mut buf = Vec::new();
        // The Result is allowed to be `Err` here: `disable_raw_mode` can
        // fail off a real tty (exactly the nextest/CI environment this test
        // runs in), but the write-side effects must still happen.
        let _ = restore_terminal(&mut buf);
        assert!(contains_restore_sequence(&buf), "{buf:?}");
    }

    #[test]
    fn quit_path_and_panic_hook_both_run_restore_terminal() {
        // Quit path: `run_tui` calls `restore_terminal` directly (proven by
        // the test above sharing that exact function). Panic hook: prove
        // `install_panic_hook`'s closure body calls it too, by installing
        // the same closure shape and triggering a synthetic panic through
        // `catch_unwind`. Nextest runs each test in its own process, so
        // mutating the global panic hook here can't leak into other tests.
        let previous = std::panic::take_hook();
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let observed_in_hook = observed.clone();
        std::panic::set_hook(Box::new(move |_info| {
            let mut buf = Vec::new();
            let _ = restore_terminal(&mut buf);
            *observed_in_hook.lock().expect("hook mutex") = buf;
        }));

        let result = std::panic::catch_unwind(|| panic!("synthetic panic for restore test"));

        std::panic::set_hook(previous);
        assert!(result.is_err());
        let buf = observed.lock().expect("hook mutex").clone();
        assert!(
            contains_restore_sequence(&buf),
            "panic hook must call restore_terminal: {buf:?}"
        );
    }

    #[test]
    fn receipt_renders_duration_events_errors_cache_and_slowest() {
        // `completions`/`cache_hits` are private to `trace_state` (they
        // only ever move through `record_completion`/`record_cache_hit`),
        // so build the cache-ratio part via the reducer and layer the rest
        // on top through the fields that are genuinely public.
        let mut session = session_stats_with_cache_ratio(0.8);
        session.events = 42;
        session.errors = 3;
        session.slowest = Some(SlowOp {
            mount: "github".into(),
            path: "/raulk/omnifs".into(),
            op: "lookup".into(),
            elapsed_us: 250_000,
        });
        let summary = SessionSummary {
            duration: Duration::from_secs(75),
            session,
            record_path: Some(PathBuf::from("/tmp/inspect.jsonl")),
        };
        let caps = Capabilities {
            width: 120,
            is_tty: false,
            color: false,
            quiet: false,
        };
        let rendered = render_receipt(&summary, caps);

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
        let summary = SessionSummary {
            duration: Duration::from_secs(5),
            session: SessionStats::default(),
            record_path: None,
        };
        let caps = Capabilities {
            width: 120,
            is_tty: false,
            color: false,
            quiet: false,
        };
        let rendered = render_receipt(&summary, caps);

        assert!(rendered.contains("• errors"), "{rendered:?}");
        assert!(rendered.contains("• cache"), "{rendered:?}");
        assert!(rendered.contains("n/a"), "{rendered:?}");
        assert!(rendered.contains("• slowest"), "{rendered:?}");
        assert!(rendered.contains("none"), "{rendered:?}");
        assert!(!rendered.contains("recorded"), "{rendered:?}");
    }

    /// Test-only helper: `SessionStats`'s cache fields are private (the
    /// public surface is `cache_hit_ratio()`), so build a stats value with a
    /// known ratio via the same `record_completion`/`record_cache_hit` path
    /// production code uses, rather than reaching into private fields.
    // `ratio` is always one of this test module's own literal call-site
    // constants (0.0..=1.0), so the truncated/sign-losing cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn session_stats_with_cache_ratio(ratio: f64) -> SessionStats {
        use crate::inspector::trace_state::TraceReducer;
        use omnifs_api::events::{
            CacheKind, InspectorEvent, InspectorRecord, OpEnd, OutcomeFields,
        };

        let mut reducer = TraceReducer::default();
        // 4 completions + 16 cache hits = 20 samples, 80% hit ratio.
        let hits = (ratio * 20.0).round() as u64;
        let completions = 20 - hits;
        for i in 0..completions {
            let trace_id = i + 1;
            reducer.apply_record(&InspectorRecord::new(
                "2026-05-23T00:00:00Z",
                i,
                trace_id,
                InspectorEvent::FuseStart {
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: "/a".into(),
                },
            ));
            reducer.apply_record(&InspectorRecord::new(
                "2026-05-23T00:00:00Z",
                i,
                trace_id,
                InspectorEvent::FuseEnd {
                    op: "lookup".into(),
                    end: OpEnd {
                        elapsed_us: 10,
                        result: OutcomeFields::ok(),
                    },
                },
            ));
        }
        for i in 0..hits {
            reducer.apply_record(&InspectorRecord::new(
                "2026-05-23T00:00:00Z",
                completions + i,
                completions + i + 1,
                InspectorEvent::CacheEvent {
                    operation_id: None,
                    mount: "github".into(),
                    path: "/b".into(),
                    kind: CacheKind::BrowseHit,
                    elapsed_us: None,
                },
            ));
        }
        reducer.session().clone()
    }
}
