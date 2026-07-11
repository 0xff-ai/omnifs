//! The closed output vocabulary: the one owner of every glyph and every color
//! role.
//!
//! Color is information, never decoration. The trio (green/yellow/red) lives on
//! the glyph column only; shape carries severity so `NO_COLOR` and color-blind
//! readers lose nothing. Cyan is the single accent, reserved for things the user
//! can act on (commands, URLs). Bold marks identity (names). Dim is the stage
//! machinery (durations, digests, hints). Everything else is the default
//! foreground.
//!
//! ANSI-16 only: the user's terminal theme picks the hue, so the functions below
//! name a role, not a color. Emission goes out through `anstream`, which strips
//! the sequences when the sink is not a color-aware TTY.

use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::OnceLock;

use owo_colors::OwoColorize;
use serde::Serialize;

/// Whether ANSI styling should be emitted at all, decided once per process.
///
/// The flat register prints through `anstream`, which strips color for a
/// non-TTY sink, but the rail register prints through cliclack/`console`, which
/// passes our pre-styled strings through verbatim. Gating the color roles here
/// makes both registers honor the same decision, so piped output and `NO_COLOR`
/// are plain everywhere, not just on the flat path. Based on stdout because the
/// state reports (the primary colored output) go there.
fn color_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        if std::env::var_os("CLICOLOR_FORCE").is_some_and(|value| value != "0") {
            return true;
        }
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        std::io::stdout().is_terminal()
    })
}

/// Apply an owo styling only when color is enabled; otherwise return the plain
/// text, so no ANSI ever reaches a non-color sink.
fn paint(s: impl Display, style: impl FnOnce(&str) -> String) -> String {
    let s = s.to_string();
    if color_enabled() { style(&s) } else { s }
}

// Color roles. These are the only place in the crate that names a color; command
// code asks for a role, never a hue.

/// Healthy / done. Semantic trio, glyph column only.
pub(crate) fn success(s: impl Display) -> String {
    paint(s, |t| t.green().to_string())
}

/// Needs attention. Semantic trio, glyph column only.
pub(crate) fn warn(s: impl Display) -> String {
    paint(s, |t| t.yellow().to_string())
}

/// Failed. Semantic trio, glyph column only.
pub(crate) fn error(s: impl Display) -> String {
    paint(s, |t| t.red().to_string())
}

/// Stage machinery: durations, digests, hints, skipped rows, the rail itself.
pub(crate) fn dim(s: impl Display) -> String {
    paint(s, |t| t.dimmed().to_string())
}

/// The one accent, one job: things the user can type, open, or answer.
pub(crate) fn accent(s: impl Display) -> String {
    paint(s, |t| t.cyan().to_string())
}

/// Identity: mount names, provider names, intro/outro lines.
pub(crate) fn bold(s: impl Display) -> String {
    paint(s, |t| t.bold().to_string())
}

/// Section heading: bold plus a structural blue so a section title separates
/// from its rows at a glance. Blue is kept distinct from the cyan accent (which
/// means "actionable") so headings never read as something to type.
pub(crate) fn heading(s: impl Display) -> String {
    paint(s, |t| t.blue().bold().to_string())
}

/// Render inline command spans written as `` `cmd` `` in the cyan accent,
/// dropping the backticks. Output is text, not markdown: a command the user can
/// type is shown by color, never by punctuation. Unbalanced backticks are left
/// literal.
pub(crate) fn accentuate(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        if let Some(close) = after.find('`') {
            out.push_str(&accent(&after[..close]));
            rest = &after[close + 1..];
        } else {
            out.push('`');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The closed glyph set. One owner per meaning; a single space always follows a
/// glyph in a row. The liveness dots (`LiveDot`/`IdleDot`) are for status lists
/// only and never carry failure: failures get `Warn`/`Fail` so shape carries
/// severity independent of color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Glyph {
    /// `✓` green: done / healthy.
    Done,
    /// `!` yellow: warn / needs attention (the one warning shape).
    Warn,
    /// `✗` red: failed.
    Fail,
    /// `•` dim: skipped / informational.
    Skip,
    /// `●` green: live liveness dot (attached), status lists only.
    LiveDot,
    /// `◌` dim: idle liveness dot (no auth needed / idle), status lists only.
    IdleDot,
    /// `-` dim: planned removal in a consent plan; flips to `Done`/`Warn` in the
    /// receipt. Consumed by the consent kit in a later wave.
    #[allow(dead_code)]
    Plan,
}

impl Glyph {
    /// The bare glyph, one visible column, no color.
    pub(crate) const fn symbol(self) -> &'static str {
        match self {
            Self::Done => "✓",
            Self::Warn => "!",
            Self::Fail => "✗",
            Self::Skip => "•",
            Self::LiveDot => "●",
            Self::IdleDot => "◌",
            Self::Plan => "-",
        }
    }

    /// The glyph colored by its role. Only the glyph carries ANSI so a caller can
    /// pad the plain key beside it without disturbing column math.
    pub(crate) fn render(self) -> String {
        match self {
            Self::Done | Self::LiveDot => success(self.symbol()),
            Self::Warn => warn(self.symbol()),
            Self::Fail => error(self.symbol()),
            Self::Skip | Self::IdleDot | Self::Plan => dim(self.symbol()),
        }
    }

    /// Stable machine name for the row's severity, used by the `Report` JSON
    /// path and by NDJSON renderers.
    pub(crate) const fn json_state(self) -> &'static str {
        match self {
            Self::Done | Self::LiveDot => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip | Self::IdleDot => "skip",
            Self::Plan => "plan",
        }
    }
}

/// Render a digest short and dim for human output: the first eight hex
/// characters. The full digest is machine-only (`--json`).
// Consumed by the provider list migration in a later cli-redesign wave.
#[allow(dead_code)]
pub(crate) fn short_digest(digest: &str) -> String {
    let short: String = digest.chars().take(8).collect();
    dim(short)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_digest_takes_eight_chars() {
        let full = "2867dacb1f9e0a5c";
        // Strip color to assert on the visible text.
        let rendered = short_digest(full);
        assert!(rendered.contains("2867dacb"));
        assert!(!rendered.contains("1f9e"));
    }

    #[test]
    fn every_glyph_is_one_visible_column() {
        for glyph in [
            Glyph::Done,
            Glyph::Warn,
            Glyph::Fail,
            Glyph::Skip,
            Glyph::LiveDot,
            Glyph::IdleDot,
            Glyph::Plan,
        ] {
            assert_eq!(glyph.symbol().chars().count(), 1, "{glyph:?}");
        }
    }
}
