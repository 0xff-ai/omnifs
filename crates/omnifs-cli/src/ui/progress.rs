//! The flat-register progress skin.
//!
//! [`LiveRow`] is one ledger row that animates in place while a step runs, then
//! settles into a static past-tense line that becomes the permanent transcript.
//! Three modes share one row: a plain spinner, an elapsed ticker
//! (`booting… 12s`), and a determinate byte/count meter (`148 MB / 262 MB`).
//!
//! Motion is a stderr-TTY courtesy: the spinner appears only once a step is
//! still live after ~150 ms, so fast steps never flicker, and a non-terminal
//! stderr shows nothing until the settle line. There is no background thread;
//! callers advance the row at natural progress points. The settle line flows
//! through [`LedgerRenderer`] so the renderer split holds: a future rail or
//! NDJSON renderer sees the same [`UiEvent`]s.

// This module is the sanctioned output owner; the drift gate denies print
// macros everywhere else.
#![allow(clippy::disallowed_macros, clippy::print_stderr)]

use std::io::{IsTerminal, Write as _};
use std::time::{Duration, Instant};

use crossterm::{
    queue,
    terminal::{Clear, ClearType},
};

use super::KEY_WIDTH;
use super::event::{LedgerRenderer, Render, UiEvent};
use super::style::{self, Glyph};

/// Braille spinner frames, advanced by [`LiveRow::update`].
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A spinner never appears before a step has run this long, so a step that
/// finishes quickly just prints its settle line.
const APPEARANCE_DELAY: Duration = Duration::from_millis(150);

/// The transient in-place line: `  <spinner> <key padded>text`, on the same grid
/// as a settled row so the spinner and the line that replaces it align. The
/// spinner is dim (machinery), never the cyan accent, which is reserved for
/// actionable things.
fn spinner_line(frame: &str, key: &str, text: &str) -> String {
    let key_pad = KEY_WIDTH.saturating_sub(key.chars().count()).max(1);
    format!(
        "  {} {key}{:pad$}{text}",
        style::dim(frame),
        "",
        pad = key_pad
    )
}

/// Render a byte count in decimal units (`148 MB`), matching the labels an image
/// registry reports.
// The determinate meters land in the flat-progress adoption wave (T3).
#[allow(dead_code)]
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

pub(crate) struct LiveRow<R = LedgerRenderer> {
    key: String,
    tty: bool,
    started: Instant,
    frame: usize,
    /// Whether a transient spinner line is currently on screen and must be
    /// cleared before the settle line is written.
    drawn: bool,
    renderer: R,
}

impl LiveRow<LedgerRenderer> {
    /// Begin a live row. Nothing is drawn yet: a fast step must not flicker, so
    /// the spinner waits for [`APPEARANCE_DELAY`] and the first [`update`].
    ///
    /// [`update`]: LiveRow::update
    pub(crate) fn start(key: &str, _text: &str) -> Self {
        Self::with_renderer(key, LedgerRenderer)
    }
}

impl<R: Render> LiveRow<R> {
    /// Begin a live row whose events are delivered to `renderer`. Later skins
    /// and the NDJSON stream can consume the same progress lifecycle without
    /// replacing this state machine.
    pub(crate) fn with_renderer(key: &str, renderer: R) -> Self {
        Self {
            key: key.to_string(),
            tty: std::io::stderr().is_terminal(),
            started: Instant::now(),
            frame: 0,
            drawn: false,
            renderer,
        }
    }

    /// Repaint the spinner with new text. A no-op until the step has been live
    /// past the appearance delay, and a no-op on a non-terminal stderr.
    pub(crate) fn update(&mut self, text: &str) {
        self.renderer.event(&UiEvent::Progress {
            key: self.key.clone(),
            message: text.to_string(),
        });
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

    /// Elapsed-ticker mode: `booting… 12s`. The verb is present-continuous while
    /// live; the settle line supplies past tense.
    // Adopted by the long-running commands (guest pull, wait_for_mount) in T3.
    #[allow(dead_code)]
    pub(crate) fn update_elapsed(&mut self, verb: &str) {
        let secs = self.started.elapsed().as_secs();
        self.update(&format!("{verb}… {secs}s"));
    }

    /// Determinate byte meter: `148 MB / 262 MB`.
    #[allow(dead_code)] // Guest image pull (T3).
    pub(crate) fn update_bytes(&mut self, done: u64, total: u64) {
        self.update(&format!("{} / {}", human_bytes(done), human_bytes(total)));
    }

    /// Determinate count meter: `3 / 10 files`.
    #[allow(dead_code)] // snapshot export (T3).
    pub(crate) fn update_count(&mut self, done: u64, total: u64, unit: &str) {
        self.update(&format!("{done} / {total} {unit}"));
    }

    pub(crate) fn settle_ok(self, value: impl std::fmt::Display) {
        self.settle(Glyph::Done, value);
    }

    pub(crate) fn settle_warn(self, value: impl std::fmt::Display) {
        self.settle(Glyph::Warn, value);
    }

    #[allow(dead_code)] // Failure settles land with the progress-adoption wave (T3).
    pub(crate) fn settle_fail(self, value: impl std::fmt::Display) {
        self.settle(Glyph::Fail, value);
    }

    /// Clear any transient line, then emit the static settle row through the
    /// renderer so the permanent record and the event stream stay in step.
    fn settle(mut self, glyph: Glyph, value: impl std::fmt::Display) {
        if self.drawn {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r");
            let _ = queue!(err, Clear(ClearType::CurrentLine));
            let _ = err.flush();
        }
        self.renderer.event(&UiEvent::RowSettled {
            glyph,
            key: std::mem::take(&mut self.key),
            value: value.to_string(),
            fix: None,
            duration: Some(self.started.elapsed()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::strip_ansi;

    #[test]
    fn human_bytes_uses_decimal_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(148_000_000), "148 MB");
        assert_eq!(human_bytes(262_000_000), "262 MB");
    }

    #[test]
    fn spinner_line_aligns_to_the_grid() {
        let plain = strip_ansi(&spinner_line("⠋", "daemon", "starting"));
        let prefix: String = plain.chars().take(18).collect();
        assert_eq!(prefix.chars().count(), 18, "{plain:?}");
        assert_eq!(plain.chars().nth(18), Some('s'), "{plain:?}");
    }

    #[test]
    fn non_tty_stays_silent_until_settle() {
        // Under nextest stderr is captured, so this exercises the non-terminal
        // path: start and update draw nothing; settle emits the static row.
        let mut row = LiveRow::start("daemon", "starting");
        assert!(!row.tty, "captured stderr must be treated as non-terminal");
        row.update("still going");
        row.update_elapsed("booting");
        assert!(!row.drawn, "no spinner may be drawn off a terminal");
        row.settle_ok("running");
    }

    #[derive(Default)]
    struct EventRecorder(Vec<UiEvent>);

    impl Render for &mut EventRecorder {
        fn event(&mut self, event: &UiEvent) {
            self.0.push(event.clone());
        }
    }

    #[test]
    fn custom_renderer_receives_progress_and_timed_settlement() {
        let mut recorder = EventRecorder::default();
        {
            let mut row = LiveRow::with_renderer("image", &mut recorder);
            row.update_bytes(148_000_000, 262_000_000);
            row.settle_ok("downloaded");
        }
        assert!(matches!(recorder.0[0], UiEvent::Progress { .. }));
        assert!(matches!(
            recorder.0[1],
            UiEvent::RowSettled {
                duration: Some(_),
                ..
            }
        ));
    }
}
