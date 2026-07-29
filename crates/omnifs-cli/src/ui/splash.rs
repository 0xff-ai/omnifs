//! The one-time `omnifs setup` splash banner. No other command
//! ever prints a banner; [`should_splash`] is the single gate that keeps it
//! that way across every output mode and terminal state this crate supports.
//! The reveal itself runs on `prompt.rs`'s raw-mode primitives
//! ([`prompt::RawTerminal`], [`prompt::redraw`], [`prompt::erase`]) and stays
//! untested here for the same reason `run_prompt_loop` does: a live terminal
//! loop cannot run under `cargo nextest` without a PTY. `should_splash` and
//! [`fits`] are the pure boundary, and together they carry the whole
//! suppression matrix.

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use super::prompt::{self, Canceled};
use super::render::Capabilities;
use super::style::{self, Stream};

/// The static banner, drawn as one block with no letter-by-letter reveal.
/// Every line is dim except [`TAGLINE_LINE`], which renders bold.
const BANNER: [&str; 9] = [
    "        ·         .              *              ·",
    "   ⋆                     .                 .",
    "                                                       *",
    "              ╔═╗ ╔╦╗ ╔╗╔ ╦ ╔═╗ ╔═╗",
    "   ·          ║ ║ ║║║ ║║║ ║ ╠╣  ╚═╗               ⋆",
    "              ╚═╝ ╩ ╩ ╝╚╝ ╩ ╚   ╚═╝",
    "                                                       .",
    "        *           open a path, read the world.",
    "   ·         ⋆               .              *",
];

/// The one line in [`BANNER`] that renders bold instead of dim.
const TAGLINE_LINE: &str = "        *           open a path, read the world.";

const HOLD: Duration = Duration::from_millis(1300);

/// Whether the splash may draw at all: a real stderr TTY, human
/// non-quiet output, and an interactive run. Pure so the whole suppression
/// matrix ("non-TTY stderr, --quiet, --no-input, structured output") is
/// testable without a terminal.
pub(crate) fn should_splash(caps: Capabilities, no_input: bool, structured: bool) -> bool {
    caps.is_tty && !caps.quiet && !no_input && !structured
}

/// The banner's widest line, in terminal columns.
fn banner_width() -> usize {
    BANNER
        .iter()
        .map(|line| super::render::display_width(line))
        .max()
        .unwrap_or(0)
}

/// Whether `caps`'s terminal is wide enough to show [`BANNER`] without
/// wrapping. A terminal narrower than the banner skips it entirely rather
/// than letting it wrap into an unreadable mess.
fn fits(caps: Capabilities) -> bool {
    caps.width >= banner_width()
}

/// Draw the `omnifs setup` splash banner if the terminal allows it, then
/// dissolve it completely before the first prompt draws; a no-op under any
/// of [`should_splash`]'s suppression conditions or when the terminal is too
/// narrow for the banner ([`fits`]). Ctrl-C during the hold cancels the whole
/// command through the same path as every other prompt
/// (`ui::prompt::Canceled`, caught at the top level); any other key just
/// skips the hold.
pub(crate) fn show(caps: Capabilities, no_input: bool, structured: bool) -> anyhow::Result<()> {
    if !should_splash(caps, no_input, structured) || !fits(caps) {
        return Ok(());
    }
    let _raw = prompt::RawTerminal::enter()?;
    run()
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

fn run() -> anyhow::Result<()> {
    let stream = Stream::Stderr;
    let lines: Vec<String> = BANNER
        .iter()
        .map(|line| {
            if *line == TAGLINE_LINE {
                style::bold(*line, stream)
            } else {
                style::dim(*line, stream)
            }
        })
        .collect();
    let mut drawn = 0usize;
    prompt::redraw(&mut drawn, &lines)?;
    match poll_interrupt(HOLD)? {
        None | Some(Interrupt::Skip) => {},
        Some(Interrupt::Cancel) => {
            prompt::erase(drawn)?;
            return Err(Canceled.into());
        },
    }
    prompt::erase(drawn)?;
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
    fn splash_policy_matrix() {
        for (is_tty, quiet, no_input, structured, expected) in [
            (true, false, false, false, true),
            (false, false, false, false, false),
            (true, true, false, false, false),
            (true, false, true, false, false),
            (true, false, false, true, false),
        ] {
            assert_eq!(
                should_splash(caps(is_tty, quiet), no_input, structured),
                expected
            );
        }
    }

    #[test]
    fn banner_skips_a_terminal_narrower_than_its_widest_line() {
        let width = banner_width();
        assert!(fits(Capabilities {
            width,
            ..caps(true, false)
        }));
        assert!(!fits(Capabilities {
            width: width - 1,
            ..caps(true, false)
        }));
    }
}
