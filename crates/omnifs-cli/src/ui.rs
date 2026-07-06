//! Terminal design system for the guided CLI surfaces.
//!
//! One alignment grid for the whole wizard: a 2-space gutter, a glyph
//! column, a fixed key column, then the value. Notes indent to the value
//! column. Color goes through `crate::style` so `NO_COLOR` and non-TTY
//! stripping keep working through `anstream`.

pub(crate) mod picker;

use std::io::{IsTerminal, Write as _};

use crossterm::{
    queue,
    terminal::{Clear, ClearType},
};

use crate::style;

pub(crate) const KEY_WIDTH: usize = 14; // ledger key column
pub(crate) const RULE_WIDTH: usize = 56; // hairline width

/// Column at which every value/note aligns: 2 gutter + 1 glyph + 1 space + key.
const VALUE_COLUMN: usize = 2 + 1 + 1 + KEY_WIDTH;
/// Command column width for `hint` rows.
const HINT_WIDTH: usize = 16;

/// One ledger row: `  <glyph> <key padded>value`. The glyph is pre-colored (one
/// visible column); padding happens on the plain key so the value lands at
/// [`VALUE_COLUMN`] regardless of color.
fn row(glyph: &str, key: &str, value: impl std::fmt::Display) -> String {
    let key_pad = KEY_WIDTH.saturating_sub(key.chars().count());
    format!("  {glyph} {key}{:pad$}{value}", "", pad = key_pad)
}

// "  ✓ key           value"  (value starts at column 2+2+KEY_WIDTH = 18)
pub(crate) fn ok(key: &str, value: impl std::fmt::Display) -> String {
    row(&style::success("✓"), key, value)
}

pub(crate) fn warn_row(key: &str, value: impl std::fmt::Display) -> String {
    row(&style::warn("!"), key, value)
}

pub(crate) fn fail(key: &str, value: impl std::fmt::Display) -> String {
    row(&style::error("✗"), key, value)
}

pub(crate) fn skip(key: &str, value: impl std::fmt::Display) -> String {
    row(&style::dim("•"), key, value)
}

/// Dim continuation aligned to the value column.
pub(crate) fn note(text: impl std::fmt::Display) -> String {
    format!("{:pad$}{}", "", style::dim(text), pad = VALUE_COLUMN)
}

/// Bold heading, flush left.
pub(crate) fn heading(text: &str) -> String {
    style::bold(text)
}

/// Dim hairline with an embedded title, padded to [`RULE_WIDTH`] chars.
pub(crate) fn rule(title: &str) -> String {
    let head = format!("── {title} ");
    let dashes = RULE_WIDTH.saturating_sub(head.chars().count());
    style::dim(format!("{head}{}", "─".repeat(dashes)))
}

/// A [`rule`] with a stage counter embedded: `── 2/6 runtime ───…`, padded to
/// [`RULE_WIDTH`]. Reuses `rule`'s construction so the hairline stays identical.
pub(crate) fn stage_rule(n: usize, total: usize, title: &str) -> String {
    rule(&format!("{n}/{total} {title}"))
}

/// Braille spinner frames advanced by [`LiveRow::update`].
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The in-place row body: `  <glyph> <key padded>text`, on the same grid as the
/// ledger rows so a settled row lines up with the spinner it replaces.
fn live_line(glyph: &str, key: &str, text: &str) -> String {
    row(&style::accent(glyph), key, text)
}

/// A single ledger row that updates in place while a long operation runs, then
/// settles into a static ledger row so the transcript matches the plain output.
///
/// On a non-terminal stderr the spinner would be noise, so `start` and every
/// `update` are no-ops there and only `settle` emits a row, keeping a captured
/// or redirected stderr to one clean line. There is no background thread or
/// timer: callers advance the spinner at natural progress points by calling
/// `update`.
pub(crate) struct LiveRow {
    key: String,
    tty: bool,
    frame: usize,
}

impl LiveRow {
    pub(crate) fn start(key: &str, text: &str) -> Self {
        let tty = std::io::stderr().is_terminal();
        if tty {
            let mut err = std::io::stderr();
            let _ = write!(err, "{}", live_line(SPINNER_FRAMES[0], key, text));
            let _ = err.flush();
        }
        // Non-TTY: emit nothing at start; the settle row is the only line.
        Self {
            key: key.to_string(),
            tty,
            frame: 0,
        }
    }

    /// Repaint the same line with the next spinner frame and new text.
    pub(crate) fn update(&mut self, text: &str) {
        if !self.tty {
            return;
        }
        self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
        let mut err = std::io::stderr();
        let _ = write!(err, "\r");
        let _ = queue!(err, Clear(ClearType::CurrentLine));
        let _ = write!(
            err,
            "{}",
            live_line(SPINNER_FRAMES[self.frame], &self.key, text)
        );
        let _ = err.flush();
    }

    pub(crate) fn settle_ok(self, value: impl std::fmt::Display) {
        let rendered = ok(&self.key, value);
        self.settle(&rendered);
    }

    pub(crate) fn settle_warn(self, value: impl std::fmt::Display) {
        let rendered = warn_row(&self.key, value);
        self.settle(&rendered);
    }

    /// Replace the live line (TTY) or simply append (non-TTY) with the final
    /// static ledger row.
    fn settle(self, rendered: &str) {
        if self.tty {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r");
            let _ = queue!(err, Clear(ClearType::CurrentLine));
            let _ = err.flush();
        }
        anstream::eprintln!("{rendered}");
    }
}

/// Command hint row: `  <cmd padded to 16><desc>`.
pub(crate) fn hint(cmd: &str, desc: &str) -> String {
    // A command longer than the column still needs a gap before the desc.
    let cmd_pad = HINT_WIDTH.saturating_sub(cmd.chars().count()).max(1);
    format!(
        "  {}{:pad$}{}",
        style::accent(cmd),
        "",
        style::dim(desc),
        pad = cmd_pad
    )
}

/// Convert an inquire prompt error into the shared cancel marker so Esc
/// (`OperationCanceled`) and Ctrl-C (`OperationInterrupted`) behave the same
/// across the custom [`picker`] and every inquire prompt: both surface as
/// [`picker::Canceled`], which the top-level handler renders as a quiet
/// `canceled` line. Non-cancel inquire errors keep their message.
pub(crate) fn from_inquire(error: inquire::InquireError) -> anyhow::Error {
    match error {
        inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted => {
            anyhow::Error::new(picker::Canceled)
        },
        other => anyhow::anyhow!("{other}"),
    }
}

/// Install the inquire theme. Call once from `main` before any prompt so every
/// prompt matches the ledger palette.
pub(crate) fn install_prompt_theme() {
    use inquire::ui::{Color, RenderConfig, StyleSheet, Styled};

    let dim = StyleSheet::new().with_fg(Color::DarkGrey);
    let cyan = StyleSheet::new().with_fg(Color::LightCyan);

    let config = RenderConfig::default()
        .with_prompt_prefix(Styled::new("?").with_fg(Color::LightGreen))
        .with_answered_prompt_prefix(Styled::new("✓").with_fg(Color::LightGreen))
        .with_highlighted_option_prefix(Styled::new("❯").with_fg(Color::LightCyan))
        .with_selected_checkbox(Styled::new("◉").with_fg(Color::LightCyan))
        .with_unselected_checkbox(Styled::new("◯").with_fg(Color::DarkGrey))
        .with_help_message(dim)
        .with_answer(cyan)
        .with_canceled_prompt_indicator(Styled::new("(canceled)").with_fg(Color::DarkGrey));

    inquire::set_global_render_config(config);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip SGR ANSI escape sequences so column math can be asserted on the
    /// plain glyphs the design system aligns.
    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                // Consume until a letter terminates the CSI sequence.
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn row_primitives_align_value_at_column_18() {
        assert_eq!(VALUE_COLUMN, 18);
        for rendered in [
            ok("environment", "macOS"),
            warn_row("daemon", "not running"),
            fail("github", "auth failed"),
            skip("linear", "skipped"),
        ] {
            let plain = strip_ansi(&rendered);
            // Everything up to the value is exactly 18 columns.
            assert!(plain.chars().count() > 18, "row too short: {plain:?}");
            let prefix: String = plain.chars().take(18).collect();
            assert_eq!(prefix.chars().count(), 18, "row {plain:?}");
            // The 18th char begins the value (non-space).
            let value_start = plain.chars().nth(18).unwrap();
            assert_ne!(value_start, ' ', "value must start at column 18: {plain:?}");
        }
    }

    #[test]
    fn note_aligns_to_value_column() {
        let plain = strip_ansi(&note("hello"));
        let prefix: String = plain.chars().take(18).collect();
        assert!(
            prefix.chars().all(|c| c == ' '),
            "note prefix not blank: {plain:?}"
        );
        assert_eq!(plain.chars().nth(18), Some('h'));
    }

    #[test]
    fn rule_is_exactly_rule_width() {
        let plain = strip_ansi(&rule("github"));
        assert_eq!(plain.chars().count(), RULE_WIDTH, "{plain:?}");
        assert!(plain.starts_with("── github "));
    }

    #[test]
    fn stage_rule_embeds_counter_and_keeps_width() {
        let plain = strip_ansi(&stage_rule(2, 6, "runtime"));
        assert_eq!(plain.chars().count(), RULE_WIDTH, "{plain:?}");
        assert!(plain.starts_with("── 2/6 runtime "), "{plain:?}");
    }

    #[test]
    fn live_line_aligns_to_the_ledger_grid() {
        let plain = strip_ansi(&live_line("⠋", "daemon", "starting"));
        let prefix: String = plain.chars().take(18).collect();
        assert_eq!(prefix.chars().count(), 18, "{plain:?}");
        assert_eq!(plain.chars().nth(18), Some('s'), "{plain:?}");
    }

    #[test]
    fn live_row_non_tty_path_is_a_noop_until_settle() {
        // Under nextest stderr is captured (not a terminal), so this exercises
        // the non-TTY fallback: start prints a note, update is a no-op, settle
        // appends the static ledger row. It must not panic or emit escapes.
        let mut row = LiveRow::start("daemon", "starting");
        assert!(!row.tty, "captured stderr must be treated as non-terminal");
        row.update("still going");
        row.settle_ok("running in docker");
    }

    #[test]
    fn hint_command_column_is_16_wide() {
        let plain = strip_ansi(&hint("omnifs shell", "browse your files"));
        // gutter(2) + command column(16) => desc starts at column 18.
        assert_eq!(plain.chars().nth(18), Some('b'), "{plain:?}");
        // A command overflowing the column keeps a one-space gap.
        let long = strip_ansi(&hint("omnifs completions", "tab completion"));
        assert!(long.contains("omnifs completions tab"), "{long:?}");
    }
}
