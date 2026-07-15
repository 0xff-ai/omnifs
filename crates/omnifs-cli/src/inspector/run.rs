//! Ratatui main loop.

use std::time::Duration;

use anyhow::{Context, anyhow};
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
        SourceKind::Socket { endpoint, .. } => Some(endpoint.label()),
        SourceKind::Replay(_) => None,
    };
    let mut app = App::new(mode, container, addr);
    let event_source = EventSource::spawn(source);

    let tick_rate = Duration::from_millis(50);
    let mut last_tick = std::time::Instant::now();

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
                last_tick = std::time::Instant::now();
            }
        }
        Ok(())
    })();

    let cleanup_result = (|| -> anyhow::Result<()> {
        disable_raw_mode().context("disable raw mode")?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )
        .context("leave alternate screen")?;
        terminal.show_cursor().context("show cursor")?;
        Ok(())
    })();
    run_result?;
    cleanup_result?;
    Ok(())
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
