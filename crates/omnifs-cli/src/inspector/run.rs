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
        SourceKind::Socket { addr, .. } => Some(addr.clone()),
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
    use super::source::SourceMessage;

    match source {
        SourceKind::Replay(path) => {
            for line in super::source::replay_file_blocking(&path)? {
                emit_plain_line(&line);
            }
        },
        SourceKind::Socket { addr, record } => {
            anstream::eprintln!("omnifs inspect: connecting to {addr}...");
            let event_source = EventSource::spawn(SourceKind::Socket {
                addr: addr.clone(),
                record,
            });
            while let Some(message) = event_source.recv() {
                match message {
                    SourceMessage::Line(line) => emit_plain_line(&line),
                    SourceMessage::Connected => {
                        anstream::eprintln!("omnifs inspect: connected to {addr}");
                    },
                    SourceMessage::Disconnected => {
                        anstream::eprintln!(
                            "omnifs inspect: disconnected from {addr}, reconnecting..."
                        );
                    },
                }
            }
        },
    }
    Ok(())
}

fn emit_plain_line(line: &str) {
    use super::format_record;
    use omnifs_api::events::InspectorRecord;

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    match InspectorRecord::parse_line(trimmed) {
        Ok(record) => anstream::println!("{}", format_record(&record)),
        Err(_) => anstream::println!("{trimmed}"),
    }
}
