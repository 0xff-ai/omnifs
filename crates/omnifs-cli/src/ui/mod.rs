//! Commands construct typed values while this module owns every byte that
//! reaches the terminal: the closed vocabulary ([`style`]), typed state
//! [`report`]s, the [`event`] model, flat [`progress`], and the cliclack
//! [`session`] rail. Stream discipline is owned here, not by commands:
//! reports go to stdout, while narration, prompts, and progress go to stderr.
//!
//! Commands that can ask a question own a [`session::Session`] for the whole
//! conversation. The small [`note`] and [`hint`] helpers remain only for
//! already-flat command surfaces and should not be used to start a rail.

// This module is the sanctioned output owner; the drift gate denies print
// macros everywhere else. Only the raw and JSON output helpers print here.
#![allow(clippy::disallowed_macros, clippy::print_stdout)]

pub(crate) mod consent;
pub(crate) mod event;
pub(crate) mod output;
pub(crate) mod picker;
pub(crate) mod progress;
pub(crate) mod prompt;
pub(crate) mod report;
pub(crate) mod session;
pub(crate) mod style;

pub(crate) use progress::LiveRow;

use std::path::PathBuf;

use anyhow::Context as _;

pub(crate) const KEY_WIDTH: usize = 14; // ledger key column

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

/// Emit a complete pre-rendered document to stdout.
pub(crate) fn print_raw(text: &str) {
    anstream::print!("{text}");
}

/// Emit pre-rendered text to stderr. The top-level error and cancel handler
/// routes through here so the crate root, which cannot carry a module-scoped
/// allow, never calls a print macro itself.
pub(crate) fn eprint_raw(text: &str) {
    anstream::eprint!("{text}");
}

/// Conversational narration on stderr, dropped under `-q`/`--quiet`. This is the
/// one path for the prose lines that lifecycle commands print directly (outside
/// the event stream); the record lines (settle rows, receipts, errors) never
/// route through here, so quiet keeps them.
pub(crate) fn narrate(line: impl std::fmt::Display) {
    if output::quiet() {
        return;
    }
    // Command spans (`` `cmd` ``) render in the cyan accent, never as literal
    // backticks: prose on the terminal is not markdown.
    anstream::eprintln!("{}", style::accentuate(&line.to_string()));
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

/// Dim continuation aligned to the value column.
pub(crate) fn note(text: impl std::fmt::Display) -> String {
    format!("{:pad$}{}", "", style::dim(text), pad = VALUE_COLUMN)
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

/// Parse a path typed at a prompt, expanding a leading `~/` when `HOME` is set.
pub(crate) fn input_path(raw: &str) -> PathBuf {
    if let Some(stripped) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(stripped);
    }
    PathBuf::from(raw)
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
    fn hint_command_column_is_16_wide() {
        let plain = strip_ansi(&hint("omnifs shell", "browse your files"));
        assert_eq!(plain.chars().nth(18), Some('b'), "{plain:?}");
        let long = strip_ansi(&hint("omnifs completions", "tab completion"));
        assert!(long.contains("omnifs completions tab"), "{long:?}");
    }
}
