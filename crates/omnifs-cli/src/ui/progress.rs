//! Short-lived progress handles returned by Output.
//!
//! The handle keeps only lifecycle facts needed to render a transient human
//! spinner. It is not a mirror of a command stage or a renderer.

#![allow(clippy::disallowed_macros, clippy::print_stderr)]

use std::io::{IsTerminal, Write as _};
use std::time::{Duration, Instant};

use crossterm::{
    queue,
    terminal::{Clear, ClearType},
};

use super::output::Output;
use super::style::{self, Glyph};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const APPEARANCE_DELAY: Duration = Duration::from_millis(150);
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);
const KEY_WIDTH: usize = 14;

fn spinner_line(frame: &str, key: &str, text: &str) -> String {
    let key_pad = KEY_WIDTH.saturating_sub(key.chars().count()).max(1);
    format!(
        "  {} {key}{:pad$}{text}",
        style::dim(frame, style::Stream::Stderr),
        "",
        pad = key_pad
    )
}
fn human_bytes(bytes: u64) -> String {
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
pub(crate) struct Progress {
    output: Output,
    key: String,
    tty: bool,
    started: Instant,
    frame: usize,
    next_update: Instant,
    drawn: bool,
}

impl Progress {
    pub(crate) fn new(output: Output, key: impl Into<String>) -> Self {
        Self {
            output,
            key: key.into(),
            tty: std::io::stderr().is_terminal(),
            started: Instant::now(),
            frame: 0,
            next_update: Instant::now(),
            drawn: false,
        }
    }

    pub(crate) fn human_bytes(bytes: u64) -> String {
        human_bytes(bytes)
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
        let mut err = std::io::stderr();
        let _ = write!(err, "\r");
        let _ = queue!(err, Clear(ClearType::CurrentLine));
        let _ = write!(
            err,
            "{}",
            spinner_line(SPINNER_FRAMES[self.frame], &self.key, text)
        );
        let _ = err.flush();
        self.drawn = true;
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
            let _ = write!(err, "\r");
            let _ = queue!(err, Clear(ClearType::CurrentLine));
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
        self.output.row(&super::report::Row::new(
            glyph,
            std::mem::take(&mut self.key),
            value,
        ));
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::strip_ansi;

    #[test]
    fn human_bytes_uses_decimal_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(148_000_000), "148 MB");
        assert_eq!(format_duration(Duration::from_millis(12)), "12ms");
        assert_eq!(format_duration(Duration::from_secs(2)), "2s");
    }

    #[test]
    fn spinner_line_aligns_to_the_grid() {
        let plain = strip_ansi(&spinner_line("⠋", "daemon", "starting"));
        assert_eq!(plain.chars().nth(18), Some('s'), "{plain:?}");
    }
}
