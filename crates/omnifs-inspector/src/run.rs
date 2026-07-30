//! Ratatui main loop.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use super::app::App;
use super::source::{EventSource, SourceKind};
use super::trace_state::SlowOp;
use super::ui;

fn restore_terminal(out: &mut impl Write) -> anyhow::Result<()> {
    let raw_mode = disable_raw_mode().context("disable raw mode");
    let screen = execute!(out, LeaveAlternateScreen, Show).context("leave alternate screen");
    raw_mode?;
    screen?;
    Ok(())
}

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

struct PanicHookGuard {
    previous: Arc<Mutex<Option<PanicHook>>>,
}

impl PanicHookGuard {
    fn install() -> Self {
        let previous = Arc::new(Mutex::new(Some(std::panic::take_hook())));
        let chained = Arc::clone(&previous);
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal(&mut std::io::stdout());
            if let Some(previous) = chained
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                previous(info);
            }
        }));
        Self { previous }
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        let _inspector_hook = std::panic::take_hook();
        if let Some(previous) = self
            .previous
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            std::panic::set_hook(previous);
        }
    }
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        let mut guard = Self { active: true };
        let mut stdout = std::io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen).context("enter alternate screen")
        {
            let _ = guard.restore(&mut stdout);
            return Err(error);
        }
        Ok(guard)
    }

    fn restore(&mut self, out: &mut impl Write) -> anyhow::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        restore_terminal(out)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore(&mut std::io::stdout());
    }
}

fn enter_terminal() -> anyhow::Result<(TerminalGuard, Terminal<CrosstermBackend<std::io::Stdout>>)>
{
    let guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let terminal = Terminal::new(backend).context("create terminal")?;
    Ok((guard, terminal))
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    event_source: &EventSource,
) -> anyhow::Result<()> {
    let tick_rate = Duration::from_millis(50);
    let mut last_tick = Instant::now();
    loop {
        terminal
            .draw(|frame| ui::render(frame, app))
            .context("draw frame")?;
        if app.quit {
            return Ok(());
        }

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));
        if event::poll(timeout).context("poll events")?
            && let Event::Key(key) = event::read().context("read event")?
            && key.kind == KeyEventKind::Press
        {
            app.handle_key(key);
            event_source.set_replay_speed(app.replay_speed);
            if let Some(path) = app.take_yank_request() {
                app.notice = match arboard::Clipboard::new()
                    .and_then(|mut clipboard| clipboard.set_text(path.clone()))
                {
                    Ok(()) => Some(format!("Copied {path}")),
                    Err(_) => Some(format!("Copy failed. Path: {path}")),
                };
            }
        }

        if last_tick.elapsed() >= tick_rate {
            for message in event_source.drain_frame() {
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
}

/// Everything the quit receipt needs, decoupled from any rendering: this
/// crate never depends on `omnifs-cli`, so `run_tui` hands back plain typed
/// data and the CLI's `commands::inspect` turns it into ledger rows through
/// its own `ui::render` helpers, exactly reproducing the prior in-crate
/// rendering.
#[derive(Debug, Clone)]
pub struct SessionReceipt {
    pub duration: Duration,
    pub events: u64,
    pub errors: u64,
    pub cache_hit_ratio: Option<f64>,
    pub slowest: Option<SlowOp>,
    pub record_path: Option<PathBuf>,
}

pub fn run_tui(
    container: String,
    source: SourceKind,
    teaching_path: Option<String>,
) -> anyhow::Result<(SessionReceipt, anyhow::Result<()>)> {
    let panic_hook = PanicHookGuard::install();
    let (mut terminal_guard, mut terminal) = enter_terminal()?;

    let is_replay = source.is_replay();
    let addr = match &source {
        SourceKind::Socket { endpoint, .. } => Some(format!("unix:{}", endpoint.display())),
        SourceKind::Replay(_) => None,
    };
    // Captured before `EventSource::spawn` moves `source`, for the quit
    // receipt's "recording to" row.
    let record_path = match &source {
        SourceKind::Socket { record, .. } => record.clone(),
        SourceKind::Replay(_) => None,
    };
    let mut app = App::new(is_replay, container, addr, teaching_path);
    let event_source = EventSource::spawn(source);
    let session_start = Instant::now();
    let run_result = run_loop(&mut terminal, &mut app, &event_source);
    let cleanup_result = terminal_guard.restore(terminal.backend_mut());
    drop(terminal);
    drop(panic_hook);

    let session = app.session();
    let receipt = SessionReceipt {
        duration: session_start.elapsed(),
        events: session.events,
        errors: session.errors,
        cache_hit_ratio: session.cache_hit_ratio(),
        slowest: session.slowest.clone(),
        record_path,
    };
    Ok((receipt, cleanup_result.and(run_result)))
}

#[derive(Debug, Clone, Copy)]
pub enum NonInteractiveFormat {
    Human,
    Jsonl,
}

/// Stream typed inspector lines to plain callbacks: `on_status` for
/// connection-state narration, `on_line` for one rendered line at a time.
/// Kept as plain closures (rather than an `omnifs-cli` `Output`/`print_raw`
/// dependency) so this crate never depends on the CLI crate; the CLI wires
/// them to `Output::narrate` and `ui::print_raw`.
pub fn run_plain(
    source: SourceKind,
    format: NonInteractiveFormat,
    mut on_status: impl FnMut(&str),
    mut on_line: impl FnMut(&str),
) -> anyhow::Result<()> {
    use super::source::SourceMessage;

    match source {
        SourceKind::Replay(path) => {
            for line in super::source::replay_file_blocking(&path)? {
                on_line(&render_plain_line(&line, format)?);
            }
        },
        SourceKind::Socket { endpoint, record } => {
            let addr = format!("unix:{}", endpoint.display());
            on_status(&format!("omnifs inspect: connecting to {addr}..."));
            let event_source = EventSource::spawn(SourceKind::Socket { endpoint, record });
            while let Some(message) = event_source.recv() {
                match message {
                    SourceMessage::Line(line) => on_line(&render_plain_line(&line, format)?),
                    SourceMessage::Connected { .. } => {
                        on_status(&format!("omnifs inspect: connected to {addr}"));
                    },
                    SourceMessage::Disconnected => {
                        on_status(&format!(
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

fn render_plain_line(
    line: &omnifs_api::events::InspectorLine,
    format: NonInteractiveFormat,
) -> anyhow::Result<String> {
    match format {
        NonInteractiveFormat::Human => Ok(format_human_line(line)),
        NonInteractiveFormat::Jsonl => line
            .to_json_line()
            .context("serialize inspector line for JSONL output"),
    }
}

fn format_human_line(line: &omnifs_api::events::InspectorLine) -> String {
    use omnifs_api::events::{InspectorEvent, InspectorLine};

    let record = match line {
        InspectorLine::Record(record) => record,
        InspectorLine::Dropped { count } => return format!("dropped {count} events\n"),
    };
    let event = match &record.event {
        InspectorEvent::FuseStart { op, mount, path } => format!("{mount} {op} {path}"),
        InspectorEvent::FuseEnd { op, end } => {
            format!("{op} {} {}us", end.result.outcome, end.elapsed_us)
        },
        InspectorEvent::ProviderStart {
            mount,
            provider,
            method,
            path,
            ..
        } => format!("{mount} {provider} {method} {path}"),
        InspectorEvent::ProviderEnd { end, .. } => {
            format!("provider {} {}us", end.result.outcome, end.elapsed_us)
        },
        InspectorEvent::CalloutStart { kind, summary, .. } => {
            format!("callout {kind} {summary}")
        },
        InspectorEvent::CalloutEnd { end, .. } => {
            format!("callout {} {}us", end.result.outcome, end.elapsed_us)
        },
        InspectorEvent::SubtreeStart { tree_ref, .. } => format!("subtree {tree_ref}"),
        InspectorEvent::SubtreeEnd { tree_ref, end, .. } => {
            format!(
                "subtree {tree_ref} {} {}us",
                end.result.outcome, end.elapsed_us
            )
        },
        InspectorEvent::CloneStart { remote, .. } => format!("clone {remote}"),
        InspectorEvent::CloneEnd { end, .. } => {
            format!("clone {} {}us", end.result.outcome, end.elapsed_us)
        },
        InspectorEvent::CacheEvent {
            mount, path, kind, ..
        } => format!("{mount} cache {kind} {path}"),
    };
    format!("{}  trace {}  {event}\n", record.ts, record.trace_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains_restore_sequence(buf: &[u8]) -> bool {
        let text = String::from_utf8_lossy(buf);
        text.contains("\u{1b}[?1049l") && text.contains("\u{1b}[?25h")
    }

    #[test]
    fn restore_terminal_leaves_the_alt_screen_and_shows_the_cursor() {
        let mut buf = Vec::new();
        // `disable_raw_mode` can fail without a real TTY, but cleanup must
        // still emit both write-side restoration commands.
        let _ = restore_terminal(&mut buf);
        assert!(contains_restore_sequence(&buf), "{buf:?}");
    }

    #[test]
    fn panic_hook_chains_and_restores_the_previous_hook() {
        let original = std::panic::take_hook();
        let previous_called = Arc::new(Mutex::new(false));
        let called_by_hook = Arc::clone(&previous_called);
        std::panic::set_hook(Box::new(move |_| {
            *called_by_hook.lock().expect("hook mutex") = true;
        }));

        let guard = PanicHookGuard::install();
        let result = std::panic::catch_unwind(|| panic!("panic while Inspector owns the hook"));
        assert!(result.is_err());
        assert!(*previous_called.lock().expect("hook mutex"));

        drop(guard);
        *previous_called.lock().expect("hook mutex") = false;
        let result = std::panic::catch_unwind(|| panic!("panic after Inspector exits"));
        assert!(result.is_err());
        assert!(*previous_called.lock().expect("hook mutex"));

        let _test_hook = std::panic::take_hook();
        std::panic::set_hook(original);
    }
}
