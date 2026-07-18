//! Commands construct typed values while this module owns every byte that
//! reaches the terminal: the closed vocabulary ([`style`]), human-only
//! responsive [`table`]s, report rows, direct [`progress`] handles, and
//! [`prompt`]s built on crossterm. Stream discipline is
//! owned here, not by commands:
//! reports go to stdout, while narration, prompts, and progress go to stderr.
//!
//! Output owns the invocation presentation lifecycle. Commands retain typed
//! plans, receipts, and domain facts, then report them through `Output`.

// This module is the sanctioned output owner; the drift gate denies print
// macros everywhere else. Only the raw and JSON output helpers print here.
#![allow(clippy::disallowed_macros, clippy::print_stdout)]

pub(crate) mod consent;
pub(crate) mod output;
pub(crate) mod progress;
pub(crate) mod prompt;
pub(crate) mod render;
pub(crate) mod report;
pub(crate) mod style;
pub(crate) mod table;

use std::path::PathBuf;

/// Command column width for `hint` rows.
const HINT_WIDTH: usize = 16;

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

/// Command hint row: `  <cmd padded to 16><desc>`. Always narration, so
/// always stderr: every caller reaches this through `Output::narrate`/`note`.
pub(crate) fn hint(cmd: &str, desc: &str) -> String {
    // A command longer than the column still needs a gap before the desc.
    let cmd_pad = HINT_WIDTH.saturating_sub(cmd.chars().count()).max(1);
    format!(
        "  {}{:pad$}{}",
        style::accent(cmd, style::Stream::Stderr),
        "",
        style::dim(desc, style::Stream::Stderr),
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

/// Strip SGR ANSI escape sequences before measuring or asserting terminal text.
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
    fn hint_command_column_is_16_wide() {
        let plain = strip_ansi(&hint("frontend shell", "browse your files"));
        assert_eq!(plain.chars().nth(18), Some('b'), "{plain:?}");
        let long = strip_ansi(&hint("omnifs completions", "tab completion"));
        assert!(long.contains("omnifs completions tab"), "{long:?}");
    }
}
