//! The one-time `omnifs setup` wordmark reveal (spec 2.5). No other command
//! ever prints a banner; [`should_splash`] is the single gate that keeps it
//! that way across every output mode and terminal state this crate supports.
//! The reveal itself follows `prompt.rs`'s raw-mode dance (save cursor, draw,
//! erase) and stays untested here for the same reason `run_prompt_loop`
//! does: a live terminal loop cannot run under `cargo nextest` without a
//! PTY. `should_splash` is the pure boundary, and it carries the whole
//! suppression matrix.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveUp, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};

use super::prompt::Canceled;
use super::render::Capabilities;
use super::style::{self, Stream};

const WORDMARK: &str = "omnifs";
const TAGLINE: &str = "your services, as files";
const LETTER_DELAY: Duration = Duration::from_millis(55);
const HOLD: Duration = Duration::from_millis(1300);

/// Whether the splash may draw at all (spec 2.5): a real stderr TTY, human
/// non-quiet output, and an interactive run. Pure so the whole suppression
/// matrix ("non-TTY stderr, --quiet, --no-input, structured output") is
/// testable without a terminal.
pub(crate) fn should_splash(caps: Capabilities, no_input: bool, structured: bool) -> bool {
    caps.is_tty && !caps.quiet && !no_input && !structured
}

/// Draw the `omnifs setup` wordmark reveal if the terminal allows it, then
/// dissolve it completely before the first prompt draws; a no-op under any
/// of [`should_splash`]'s suppression conditions. Ctrl-C during the reveal
/// cancels the whole command through the same path as every other prompt
/// (`ui::prompt::Canceled`, caught at the top level); any other key just
/// fast-forwards past the reveal.
pub(crate) fn show(caps: Capabilities, no_input: bool, structured: bool) -> anyhow::Result<()> {
    if !should_splash(caps, no_input, structured) {
        return Ok(());
    }
    enable_raw_mode()?;
    let mut out = io::stderr();
    let _ = queue!(out, Hide);
    let result = run(&mut out);
    let _ = queue!(out, Show);
    let _ = out.flush();
    let _ = disable_raw_mode();
    result
}

enum Interrupt {
    Skip,
    Cancel,
}

/// Poll for a key press within `timeout`, distinguishing Ctrl-C (cancel)
/// from every other key (skip ahead). `None` means the timeout elapsed with
/// no key pressed at all.
fn poll_interrupt(timeout: Duration) -> io::Result<Option<Interrupt>> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        if !event::poll(remaining)? {
            return Ok(None);
        }
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let cancel =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            return Ok(Some(if cancel {
                Interrupt::Cancel
            } else {
                Interrupt::Skip
            }));
        }
    }
}

/// Redraw the transient frame in place, matching `prompt.rs::redraw`: move up
/// over the previous frame, clear below the cursor, then print with explicit
/// line breaks (raw mode disables CR-on-LF translation).
fn redraw(out: &mut impl Write, drawn: &mut usize, lines: &[String]) -> io::Result<()> {
    if *drawn > 0 {
        queue!(out, MoveUp(u16::try_from(*drawn).unwrap_or(u16::MAX)))?;
    }
    queue!(out, Clear(ClearType::FromCursorDown))?;
    for line in lines {
        write!(out, "{line}\r\n")?;
    }
    out.flush()?;
    *drawn = lines.len();
    Ok(())
}

fn erase(out: &mut impl Write, drawn: usize) -> io::Result<()> {
    if drawn > 0 {
        queue!(out, MoveUp(u16::try_from(drawn).unwrap_or(u16::MAX)))?;
    }
    queue!(out, Clear(ClearType::FromCursorDown))?;
    out.flush()
}

fn run(out: &mut impl Write) -> anyhow::Result<()> {
    let stream = Stream::Stderr;
    let mut drawn = 0usize;
    let mut interrupted = false;
    for count in 1..=WORDMARK.chars().count() {
        let partial: String = WORDMARK.chars().take(count).collect();
        redraw(out, &mut drawn, &[style::bold(partial, stream)])?;
        match poll_interrupt(LETTER_DELAY)? {
            None => {},
            Some(Interrupt::Skip) => {
                interrupted = true;
                break;
            },
            Some(Interrupt::Cancel) => {
                erase(out, drawn)?;
                return Err(Canceled.into());
            },
        }
    }
    redraw(
        out,
        &mut drawn,
        &[style::bold(WORDMARK, stream), style::dim(TAGLINE, stream)],
    )?;
    if !interrupted {
        match poll_interrupt(HOLD)? {
            None | Some(Interrupt::Skip) => {},
            Some(Interrupt::Cancel) => {
                erase(out, drawn)?;
                return Err(Canceled.into());
            },
        }
    }
    erase(out, drawn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(is_tty: bool, quiet: bool) -> Capabilities {
        Capabilities {
            width: 80,
            is_tty,
            color: is_tty,
            quiet,
        }
    }

    #[test]
    fn should_splash_only_on_a_real_interactive_human_tty() {
        assert!(should_splash(caps(true, false), false, false));
    }

    #[test]
    fn should_splash_is_false_for_non_tty_stderr() {
        assert!(!should_splash(caps(false, false), false, false));
    }

    #[test]
    fn should_splash_is_false_for_quiet() {
        assert!(!should_splash(caps(true, true), false, false));
    }

    #[test]
    fn should_splash_is_false_for_no_input() {
        assert!(!should_splash(caps(true, false), true, false));
    }

    #[test]
    fn should_splash_is_false_for_structured_output() {
        assert!(!should_splash(caps(true, false), false, true));
    }
}
