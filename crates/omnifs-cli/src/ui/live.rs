#![allow(
    dead_code,
    reason = "Plan 008 replaces the former client runtime spinner with streamed daemon progress"
)]

//! The transient spinner layer: one pending operation's frame, drawn only on
//! a real TTY and never left in scrollback. [`Spinner`] owns redraw
//! throttling (an appearance delay so a fast operation never flashes, then a
//! capped redraw cadence), multi-row-aware cursor erasure before each
//! redraw, and degrading to no draw at all under `--quiet`/structured output
//! (`Output::show_progress`). It settles into a durable ledger row through
//! `Output::ledger_row` once the operation finishes; this file owns nothing
//! beyond the transient frame itself.

#![allow(clippy::disallowed_macros, clippy::print_stderr)]

use std::io::{IsTerminal, Write as _};
use std::time::{Duration, Instant};

use crossterm::{
    cursor, queue,
    terminal::{Clear, ClearType},
};

use super::output::Output;
use super::render;
use super::style::{self, Glyph};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const APPEARANCE_DELAY: Duration = Duration::from_millis(150);
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// The transient spinner frame, indented two spaces: `key_width` is the same
/// block-scoped width its settled row (via [`Spinner::settle`]) renders at,
/// so the frame never jumps column when it resolves.
fn spinner_line(frame: &str, key: &str, text: &str, key_width: usize) -> String {
    let pad = key_width.saturating_sub(render::display_width(key)) + render::LEDGER_GAP;
    format!(
        "  {} {key}{:pad$}{text}",
        style::dim(frame, style::Stream::Stderr),
        "",
        pad = pad
    )
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut unit = 0;
    let mut factor = 1_u64;
    while bytes / factor >= 1000 && unit < UNITS.len() - 1 {
        factor *= 1000;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        let rounded = bytes.saturating_add(factor / 2) / factor;
        format!("{rounded} {}", UNITS[unit])
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

/// A single pending operation: appears after a short
/// delay, redraws at a throttled cadence, and is replaced in place by its
/// durable ledger row with a dim duration suffix once it settles.
pub(crate) struct Spinner {
    output: Output,
    key: String,
    key_width: usize,
    tty: bool,
    started: Instant,
    frame: usize,
    next_update: Instant,
    drawn: bool,
    /// Physical rows the last real draw occupied (0 when nothing has been
    /// drawn on the real terminal yet). A spinner line can wrap past one
    /// column just like a multi-line frame, so the next draw must move up
    /// past every wrapped row, not just one, before overwriting it.
    drawn_rows: usize,
}

impl Spinner {
    pub(crate) fn new(output: Output, key: impl Into<String>, key_width: usize) -> Self {
        Self {
            output,
            key: key.into(),
            key_width,
            tty: std::io::stderr().is_terminal(),
            started: Instant::now(),
            frame: 0,
            next_update: Instant::now(),
            drawn: false,
            drawn_rows: 0,
        }
    }

    pub(crate) fn update(&mut self, text: &str) {
        if !self.output.show_progress() {
            return;
        }
        let now = Instant::now();
        if now < self.next_update {
            return;
        }
        self.next_update = now + UPDATE_INTERVAL;
        if !self.tty || self.started.elapsed() < APPEARANCE_DELAY {
            return;
        }
        self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
        let line = spinner_line(SPINNER_FRAMES[self.frame], &self.key, text, self.key_width);
        let mut err = std::io::stderr();
        // Move up over every row the previous draw wrapped onto (not just
        // one) before clearing: a spinner line can exceed the terminal
        // width just like a multi-line frame, and `\r` alone only returns
        // to column 0 of whatever row auto-wrap left the cursor on.
        if self.drawn_rows > 1 {
            let _ = queue!(err, cursor::MoveUp(rows(self.drawn_rows - 1)));
        }
        let _ = write!(err, "\r");
        let _ = queue!(err, Clear(ClearType::FromCursorDown));
        let _ = write!(err, "{line}");
        let _ = err.flush();
        self.drawn = true;
        self.drawn_rows = render::physical_rows(&line, render::terminal_width());
    }

    pub(crate) fn update_bytes_with(
        &mut self,
        done: u64,
        total: u64,
        context: impl std::fmt::Display,
    ) {
        self.update(&format!(
            "{} / {} {context}",
            human_bytes(done),
            human_bytes(total)
        ));
    }

    pub(crate) fn settle_ok(self, value: impl std::fmt::Display) {
        self.settle(Glyph::Done, value);
    }

    pub(crate) fn settle_warn(self, value: impl std::fmt::Display) {
        self.settle(Glyph::Warn, value);
    }

    pub(crate) fn settle_fail(self, value: impl std::fmt::Display) {
        self.settle(Glyph::Fail, value);
    }

    fn settle(mut self, glyph: Glyph, value: impl std::fmt::Display) {
        if !self.output.show_progress() {
            return;
        }
        if self.drawn {
            let mut err = std::io::stderr();
            if self.drawn_rows > 1 {
                let _ = queue!(err, cursor::MoveUp(rows(self.drawn_rows - 1)));
            }
            let _ = write!(err, "\r");
            let _ = queue!(err, Clear(ClearType::FromCursorDown));
            let _ = err.flush();
        }
        let value = value.to_string();
        let value = if self.output.is_structured() {
            value
        } else {
            format!(
                "{value} {}",
                style::dim(
                    format!("({})", format_duration(self.started.elapsed())),
                    style::Stream::Stderr
                )
            )
        };
        self.output.ledger_row(
            &render::LedgerRow::new(glyph, std::mem::take(&mut self.key), value),
            self.key_width,
        );
    }
}

/// A drawn line count clamped to `u16`'s range for a crossterm cursor move.
/// A live region realistically never draws anywhere near 65536 lines, so
/// saturating (rather than propagating a conversion error through every
/// cursor call) is the honest behavior here.
fn rows(count: usize) -> u16 {
    u16::try_from(count).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_uses_decimal_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(148_000_000), "148 MB");
        assert_eq!(format_duration(Duration::from_millis(12)), "12ms");
        assert_eq!(format_duration(Duration::from_secs(2)), "2s");
    }

    #[test]
    fn spinner_line_aligns_to_the_block_scoped_key_width() {
        // `key_width` 11 mirrors the status block (`providers`/`attachments`):
        // "daemon" (6) plus the 3-space gap this test's
        // width leaves after the wider sibling keys lands "starting" at
        // column 18 (2-space indent + 1 frame + 1 space + 6 key + 8 pad).
        let plain = super::super::strip_ansi(&spinner_line("⠋", "daemon", "starting", 11));
        assert_eq!(plain.chars().nth(18), Some('s'), "{plain:?}");
    }

    #[test]
    fn spinner_line_never_drops_the_gap_for_a_single_key_block() {
        // A standalone single-key block (`key_width` equal to the key's own
        // width) still gets the full 3-space `LEDGER_GAP`, never truncates.
        let plain = super::super::strip_ansi(&spinner_line("⠋", "daemon", "starting", 6));
        assert_eq!(plain.chars().nth(13), Some('s'), "{plain:?}");
    }
}
