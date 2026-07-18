//! The transient layer (spec 2.5): spinners, live regions, and byte
//! progress. Everything here exists only on a TTY and never survives in
//! scrollback; every primitive settles into a durable row (`ui/report.rs`)
//! once its operation finishes. This module owns cursor movement, redraw
//! throttling, non-TTY/quiet degradation to stable settle lines, and Ctrl-C
//! erasure for the whole transient layer; `ui/progress.rs` is retired into
//! this file rather than surviving as a second owner of the same concern.

#![allow(clippy::disallowed_macros, clippy::print_stderr)]

use std::io::{IsTerminal, Write as _};
use std::time::{Duration, Instant};

use crossterm::{
    cursor, queue,
    terminal::{Clear, ClearType},
};

use super::output::Output;
use super::prompt::Canceled;
use super::render;
use super::style::{self, Glyph};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const APPEARANCE_DELAY: Duration = Duration::from_millis(150);
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// The transient spinner frame, indented two spaces to match
/// [`region_frame`]'s in-flight rows (spec 2.5): `key_width` is the same
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

/// A single pending operation (spec 2.5 "Spinner"): appears after a short
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
            spinner_line(SPINNER_FRAMES[self.frame], &self.key, text, self.key_width)
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

/// One in-flight unit inside a [`LiveRegion`]: its own spinner frame and
/// label, settling in place to a `✓`/`!`/`✗` line without leaving the
/// region until the whole group finishes.
#[derive(Debug, Clone)]
struct Unit {
    key: String,
    text: String,
    settled: Option<Glyph>,
}

/// Render one live-region frame as plain text (no cursor control), one line
/// per unit at a two-space indent. Kept separate from the terminal-writing
/// path so the frame content is deterministically testable.
fn region_frame(units: &[Unit], frame_symbol: &str) -> Vec<String> {
    units
        .iter()
        .map(|unit| match unit.settled {
            Some(glyph) => format!("  {} {}", glyph.render(style::Stream::Stderr), unit.text),
            None => format!(
                "  {} {}",
                style::dim(frame_symbol, style::Stream::Stderr),
                unit.text
            ),
        })
        .collect()
}

/// Parallel operations rendered as one block of two-space-indented lines
/// (spec 2.5 "Live region"). Non-TTY or `--quiet` never draws a region at
/// all: `update`/`settle` become no-ops and [`LiveRegion::finish`] /
/// [`LiveRegion::cancel`] still emit exactly the one durable row a TTY run
/// would leave behind, so both paths agree on the record that survives.
pub(crate) struct LiveRegion {
    output: Output,
    tty: bool,
    units: Vec<Unit>,
    frame: usize,
    started: Instant,
    next_update: Instant,
    drawn_lines: usize,
}

impl LiveRegion {
    pub(crate) fn new(output: Output, keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let tty = output.show_progress() && std::io::stderr().is_terminal();
        Self {
            output,
            tty,
            units: keys
                .into_iter()
                .map(|key| Unit {
                    key: key.into(),
                    text: String::new(),
                    settled: None,
                })
                .collect(),
            frame: 0,
            started: Instant::now(),
            next_update: Instant::now(),
            drawn_lines: 0,
        }
    }

    /// Update one unit's in-flight label. A no-op once that unit has
    /// settled, and a no-op for an unknown key (the region's key set is
    /// fixed at construction).
    pub(crate) fn update(&mut self, key: &str, text: impl Into<String>) {
        if let Some(unit) = self.units.iter_mut().find(|unit| unit.key == key)
            && unit.settled.is_none()
        {
            unit.text = text.into();
        }
        self.redraw_if_due();
    }

    /// Settle one unit in place to a `✓`/`!`/`✗` line. It stays visible in
    /// the region until [`LiveRegion::finish`]/[`LiveRegion::cancel`] erases
    /// the whole group. Not yet called by a real multi-unit region: this
    /// slice's two regions (provider warmup, frontend reattachment) each
    /// track a single collapsed unit, so per-unit settling awaits a future
    /// slice that fans a region out over several concurrent items.
    #[allow(dead_code)]
    pub(crate) fn settle(&mut self, key: &str, glyph: Glyph, text: impl Into<String>) {
        if let Some(unit) = self.units.iter_mut().find(|unit| unit.key == key) {
            unit.text = text.into();
            unit.settled = Some(glyph);
        }
        self.redraw_if_due();
    }

    fn redraw_if_due(&mut self) {
        if !self.tty {
            return;
        }
        let now = Instant::now();
        if now < self.next_update {
            return;
        }
        self.next_update = now + UPDATE_INTERVAL;
        if self.started.elapsed() < APPEARANCE_DELAY {
            return;
        }
        self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
        self.draw();
    }

    fn draw(&mut self) {
        let mut err = std::io::stderr();
        if self.drawn_lines > 0 {
            let _ = queue!(err, cursor::MoveUp(rows(self.drawn_lines)));
        }
        for line in region_frame(&self.units, SPINNER_FRAMES[self.frame]) {
            let _ = queue!(err, Clear(ClearType::CurrentLine));
            let _ = write!(err, "{line}\r\n");
        }
        self.drawn_lines = self.units.len();
        let _ = err.flush();
    }

    /// Erase every drawn region line, leaving the cursor where the region
    /// started. A no-op when nothing was ever drawn (non-TTY, quiet, or a
    /// group that settled before its appearance delay elapsed).
    fn erase(&mut self) {
        if !self.tty || self.drawn_lines == 0 {
            return;
        }
        let mut err = std::io::stderr();
        let _ = queue!(err, cursor::MoveUp(rows(self.drawn_lines)));
        for _ in 0..self.drawn_lines {
            let _ = queue!(err, Clear(ClearType::CurrentLine), cursor::MoveDown(1));
        }
        let _ = queue!(err, cursor::MoveUp(rows(self.drawn_lines)));
        let _ = err.flush();
        self.drawn_lines = 0;
    }

    /// The whole group completed: erase the transient region and print
    /// exactly one durable summary row (for example
    /// `✓ providers  2/2 warm (1.2s)`), aligned to `key_width` so it lines
    /// up with the surrounding ledger block even though this row is printed
    /// well after the others in that block have already settled.
    pub(crate) fn finish(
        mut self,
        glyph: Glyph,
        key: impl Into<String>,
        value: impl Into<String>,
        key_width: usize,
    ) {
        self.erase();
        self.output
            .ledger_row(&render::LedgerRow::new(glyph, key, value), key_width);
    }

    /// Ctrl-C fired while this region was on screen: erase it and print the
    /// caller's explicit partial-state row before propagating cancellation
    /// through the existing cancel path (`main.rs` already renders
    /// `canceled` and exits 130 for `prompt::Canceled`).
    pub(crate) fn cancel(
        mut self,
        glyph: Glyph,
        key: impl Into<String>,
        value: impl Into<String>,
        key_width: usize,
    ) {
        self.erase();
        self.output
            .ledger_row(&render::LedgerRow::new(glyph, key, value), key_width);
    }

    /// Race a future against Ctrl-C while a live region may be on screen.
    /// Callers render their own partial-state summary through
    /// [`LiveRegion::cancel`] before propagating the resulting error.
    pub(crate) async fn race<T>(
        future: impl std::future::Future<Output = T>,
    ) -> Result<T, Canceled> {
        tokio::select! {
            value = future => Ok(value),
            _ = tokio::signal::ctrl_c() => Err(Canceled),
        }
    }
}

/// A `done / total` byte counter with decimal units, and (only once the
/// total is known and stable) a fixed-width bar. Spec 2.5 forbids a fake
/// percentage, so there is no bar-only rendering: a caller with an unknown
/// total uses the counter text alone (`Spinner::update_bytes_with` already
/// does this). The bar primitive is built now; its first real caller
/// (`frontend enable`'s image pull) migrates onto it in a later slice.
#[allow(dead_code)]
pub(crate) fn byte_progress_bar(done: u64, total: u64, width: usize) -> String {
    if total == 0 || width == 0 {
        return format!("{} / {}", human_bytes(done), human_bytes(total));
    }
    // Integer math instead of a float ratio: `done.min(total)` bounds the
    // numerator to `total`, so `filled` is always <= `width` and the
    // narrowing cast back to `usize` (`width`'s own type) never truncates.
    let filled_u128 = u128::from(done.min(total)) * width as u128 / u128::from(total);
    let filled = usize::try_from(filled_u128).unwrap_or(width);
    let bar = format!("{}{}", "#".repeat(filled), "-".repeat(width - filled));
    format!("[{bar}] {} / {}", human_bytes(done), human_bytes(total))
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
        // `key_width` 9 mirrors `up`'s real block (`providers`/`frontends`
        // tie at 9 chars): "daemon" (6) plus the 3-space gap this test's
        // width leaves after the wider sibling keys lands "starting" at
        // column 16 (2-space indent + 1 frame + 1 space + 6 key + 6 pad).
        let plain = super::super::strip_ansi(&spinner_line("⠋", "daemon", "starting", 9));
        assert_eq!(plain.chars().nth(16), Some('s'), "{plain:?}");
    }

    #[test]
    fn spinner_line_never_drops_the_gap_for_a_single_key_block() {
        // A standalone single-key block (`key_width` equal to the key's own
        // width) still gets the full 3-space `LEDGER_GAP`, never truncates.
        let plain = super::super::strip_ansi(&spinner_line("⠋", "daemon", "starting", 6));
        assert_eq!(plain.chars().nth(13), Some('s'), "{plain:?}");
    }

    #[test]
    fn region_frame_renders_one_indented_line_per_unit() {
        let units = [
            Unit {
                key: "github".to_owned(),
                text: "warming github".to_owned(),
                settled: None,
            },
            Unit {
                key: "linear".to_owned(),
                text: "linear warm".to_owned(),
                settled: Some(Glyph::Done),
            },
        ];
        let frame = region_frame(&units, "⠋");
        assert_eq!(frame.len(), 2);
        let pending = super::super::strip_ansi(&frame[0]);
        assert_eq!(pending, "  ⠋ warming github");
        let settled = super::super::strip_ansi(&frame[1]);
        assert_eq!(settled, "  ✓ linear warm");
    }

    #[test]
    fn byte_progress_bar_never_claims_a_percentage_without_a_total() {
        assert_eq!(byte_progress_bar(10, 0, 10), "10 B / 0 B");
        let bar = byte_progress_bar(50, 100, 10);
        assert!(bar.starts_with('['), "{bar:?}");
        assert!(bar.contains("50 B / 100 B"), "{bar:?}");
    }

    #[test]
    fn live_region_update_and_settle_are_no_ops_for_unknown_keys() {
        let output = Output::new(super::super::output::OutputMode::Human, true);
        let mut region = LiveRegion::new(output, ["frontends"]);
        region.update("missing", "ignored");
        region.settle("missing", Glyph::Done, "ignored");
        assert!(region.units.iter().all(|unit| unit.text.is_empty()));
    }
}
