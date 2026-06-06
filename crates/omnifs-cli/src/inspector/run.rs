//! Ratatui main loop.

use std::time::Duration;

use anyhow::Context;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use super::app::{App, ConnectionMode};
use super::source::{EventSource, SourceKind};
use super::ui;

pub fn run_tui(mode: ConnectionMode, container: String, source: SourceKind) -> anyhow::Result<()> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let addr = match &source {
        SourceKind::Socket { addr, .. } => Some(*addr),
        SourceKind::Replay(_) => None,
    };
    let mut app = App::new(mode, container, addr);
    let event_source = EventSource::spawn(source);

    let tick_rate = Duration::from_millis(50);
    let mut last_tick = std::time::Instant::now();

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
                app.apply_source_message(message);
            }
            app.animate();
            last_tick = std::time::Instant::now();
        }
    }

    disable_raw_mode().context("disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")?;
    Ok(())
}

pub fn run_plain(source: SourceKind) -> anyhow::Result<()> {
    use super::format_record;
    use omnifs_inspector::parse_record_line;

    let lines: Vec<String> = match source {
        SourceKind::Replay(path) => super::source::replay_file_blocking(&path)?,
        SourceKind::Socket { .. } => {
            anyhow::bail!(
                "plain mode against an inspector socket uses blocking attach in the inspect command"
            );
        },
    };
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_record_line(trimmed) {
            Ok(record) => anstream::println!("{}", format_record(&record)),
            Err(_) => anstream::println!("{trimmed}"),
        }
    }
    Ok(())
}
