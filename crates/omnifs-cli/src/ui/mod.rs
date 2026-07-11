//! The CLI output toolkit: commands construct typed values, this module owns
//! every byte that reaches the terminal.
//!
//! The flat register lives here: the closed vocabulary ([`style`]), typed state
//! [`report`]s, the [`event`] model with a flat ledger renderer, and the flat
//! [`progress`] skin. The session register (cliclack rail) plugs into the same
//! [`event`] model in a later wave. Stream discipline is owned here, not by
//! commands: reports go to stdout, narration and progress to stderr.
//!
//! Legacy free functions ([`ok`], [`note`], [`rule`], and friends) survive for
//! command files that have not yet moved onto [`report`] and the session
//! register; they render on the same grid and die as those files migrate.

// This module is the sanctioned output owner; the drift gate denies print
// macros everywhere else. Only `print_json` and `eprint_raw` print here.
#![allow(clippy::disallowed_macros, clippy::print_stdout)]

pub(crate) mod event;
pub(crate) mod picker;
pub(crate) mod progress;
pub(crate) mod report;
pub(crate) mod style;

pub(crate) use progress::LiveRow;

use std::path::PathBuf;

use anyhow::Context as _;

pub(crate) const KEY_WIDTH: usize = 14; // ledger key column
pub(crate) const RULE_WIDTH: usize = 56; // hairline width, dies with the T2 session register

/// Column at which every value/note aligns: 2 gutter + 1 glyph + 1 space + key.
const VALUE_COLUMN: usize = 2 + 1 + 1 + KEY_WIDTH;
/// Command column width for `hint` rows.
const HINT_WIDTH: usize = 16;

/// Serialize a value and print it as exactly one JSON document on stdout. The
/// single machine-output path, so `--json` commands never call a print macro
/// themselves.
pub(crate) fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    let serialized = serde_json::to_string(value).context("serialize JSON output")?;
    anstream::println!("{serialized}");
    Ok(())
}

/// Emit pre-rendered text to stderr. The top-level error and cancel handler
/// routes through here so the crate root, which cannot carry a module-scoped
/// allow, never calls a print macro itself.
pub(crate) fn eprint_raw(text: &str) {
    anstream::eprint!("{text}");
}

/// Truncate plain text to `max_chars`, counting the ellipsis in that budget.
pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        let _ = out.pop();
        out.push('…');
    }
    out
}

/// One ledger row: `  <glyph> <key padded>value`. The glyph is pre-colored (one
/// visible column); padding happens on the plain key so the value lands at
/// [`VALUE_COLUMN`] regardless of color.
fn row(glyph: &str, key: &str, value: impl std::fmt::Display) -> String {
    let key_pad = KEY_WIDTH.saturating_sub(key.chars().count());
    format!("  {glyph} {key}{:pad$}{value}", "", pad = key_pad)
}

// "  ✓ key           value"  (value starts at column 2+2+KEY_WIDTH = 18)
pub(crate) fn ok(key: &str, value: impl std::fmt::Display) -> String {
    row(&style::Glyph::Done.render(), key, value)
}

pub(crate) fn warn_row(key: &str, value: impl std::fmt::Display) -> String {
    row(&style::Glyph::Warn.render(), key, value)
}

pub(crate) fn fail(key: &str, value: impl std::fmt::Display) -> String {
    row(&style::Glyph::Fail.render(), key, value)
}

pub(crate) fn skip(key: &str, value: impl std::fmt::Display) -> String {
    row(&style::Glyph::Skip.render(), key, value)
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
///
// Dies with the T2 session register; setup and init still call it until then.
pub(crate) fn rule(title: &str) -> String {
    let head = format!("── {title} ");
    let dashes = RULE_WIDTH.saturating_sub(head.chars().count());
    style::dim(format!("{head}{}", "─".repeat(dashes)))
}

/// A [`rule`] with a stage counter embedded: `── 2/6 runtime ───…`, padded to
/// [`RULE_WIDTH`]. Reuses `rule`'s construction so the hairline stays identical.
///
// Dies with the T2 session register; setup and init still call it until then.
pub(crate) fn stage_rule(n: usize, total: usize, title: &str) -> String {
    rule(&format!("{n}/{total} {title}"))
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

/// Parse a path typed at a prompt, expanding a leading `~/` when `HOME` is set.
pub(crate) fn input_path(raw: &str) -> PathBuf {
    if let Some(stripped) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(stripped);
    }
    PathBuf::from(raw)
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

/// Strip SGR ANSI escape sequences so column math can be asserted on the plain
/// glyphs the toolkit aligns. Shared by the toolkit's grid tests.
#[cfg(test)]
pub(crate) fn strip_ansi(input: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_uses_a_total_character_budget() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello!", 5), "hell…");
        assert_eq!(truncate("éé", 3), "éé");
        assert_eq!(truncate("éééé", 3), "éé…");
        assert_eq!(truncate("🚀火é", 2), "🚀…");
        assert_eq!(truncate("hi", 1), "…");
        assert_eq!(truncate("hello", 0), "");
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
            assert!(plain.chars().count() > 18, "row too short: {plain:?}");
            let prefix: String = plain.chars().take(18).collect();
            assert_eq!(prefix.chars().count(), 18, "row {plain:?}");
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
    fn hint_command_column_is_16_wide() {
        let plain = strip_ansi(&hint("omnifs shell", "browse your files"));
        assert_eq!(plain.chars().nth(18), Some('b'), "{plain:?}");
        let long = strip_ansi(&hint("omnifs completions", "tab completion"));
        assert!(long.contains("omnifs completions tab"), "{long:?}");
    }
}
