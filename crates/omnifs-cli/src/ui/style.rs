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

/// The OS stream a piece of colored output ultimately reaches. Color is
/// decided per stream, not once per process: a report piped on stdout stays
/// plain even while a live narration line on stderr is colored, and the
/// reverse holds too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

/// How a caller supplies the color decision to a role function: either "ask
/// the real process, for this stream" (live narration, prompts, and progress,
/// which always target one real OS stream) or "use exactly this" (the flat
/// renderer and responsive tables, which take color as an injected capability
/// so tests never have to fake a TTY).
#[derive(Debug, Clone, Copy)]
pub(crate) enum ColorMode {
    Stream(Stream),
    Forced(bool),
}

impl From<Stream> for ColorMode {
    fn from(stream: Stream) -> Self {
        Self::Stream(stream)
    }
}

impl From<bool> for ColorMode {
    fn from(enabled: bool) -> Self {
        Self::Forced(enabled)
    }
}

impl ColorMode {
    fn enabled(self) -> bool {
        match self {
            Self::Stream(stream) => color_enabled(stream),
            Self::Forced(enabled) => enabled,
        }
    }
}

/// Whether ANSI styling should reach `stream`, decided once per process per
/// stream. `NO_COLOR` and `CLICOLOR_FORCE` override both streams identically;
/// absent either, each stream is gated on its own TTY-ness, so redirecting
/// one stream never silently mutes or forces color on the other.
pub(crate) fn color_enabled(stream: Stream) -> bool {
    static STDOUT: OnceLock<bool> = OnceLock::new();
    static STDERR: OnceLock<bool> = OnceLock::new();
    let cell = match stream {
        Stream::Stdout => &STDOUT,
        Stream::Stderr => &STDERR,
    };
    *cell.get_or_init(|| {
        if std::env::var_os("CLICOLOR_FORCE").is_some_and(|value| value != "0") {
            return true;
        }
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        match stream {
            Stream::Stdout => std::io::stdout().is_terminal(),
            Stream::Stderr => std::io::stderr().is_terminal(),
        }
    })
}

/// Apply an owo styling only when color is enabled; otherwise return the plain
/// text, so no ANSI ever reaches a non-color sink.
fn paint(
    s: impl Display,
    mode: impl Into<ColorMode>,
    style: impl FnOnce(&str) -> String,
) -> String {
    let s = s.to_string();
    if mode.into().enabled() { style(&s) } else { s }
}

// Color roles. These are the only place in the crate that names a color; command
// code asks for a role, never a hue.

/// Healthy / done. Semantic trio, glyph column only.
pub(crate) fn success(s: impl Display, mode: impl Into<ColorMode>) -> String {
    paint(s, mode, |t| t.green().to_string())
}

/// Needs attention. Semantic trio, glyph column only.
pub(crate) fn warn(s: impl Display, mode: impl Into<ColorMode>) -> String {
    paint(s, mode, |t| t.yellow().to_string())
}

/// Failed. Semantic trio, glyph column only.
pub(crate) fn error(s: impl Display, mode: impl Into<ColorMode>) -> String {
    paint(s, mode, |t| t.red().to_string())
}

/// Stage machinery: durations, digests, hints, skipped rows, the rail itself.
pub(crate) fn dim(s: impl Display, mode: impl Into<ColorMode>) -> String {
    paint(s, mode, |t| t.dimmed().to_string())
}

/// The one accent, one job: things the user can type, open, or answer.
pub(crate) fn accent(s: impl Display, mode: impl Into<ColorMode>) -> String {
    paint(s, mode, |t| t.cyan().to_string())
}

/// Identity: mount names, provider names, and section headings (`render.rs`'s
/// `heading` primitive uses this role, not a separate blue one: the
/// prefers plain bold for report sections).
pub(crate) fn bold(s: impl Display, mode: impl Into<ColorMode>) -> String {
    paint(s, mode, |t| t.bold().to_string())
}

/// Render inline command spans written as `` `cmd` `` in the cyan accent,
/// dropping the backticks. Output is text, not markdown: a command the user can
/// type is shown by color, never by punctuation. Unbalanced backticks are left
/// literal.
pub(crate) fn accentuate(text: &str, mode: impl Into<ColorMode>) -> String {
    let mode = mode.into();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        if let Some(close) = after.find('`') {
            out.push_str(&accent(&after[..close], mode));
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
/// glyph in a row. Failures get `Warn`/`Fail` so shape carries severity
/// independent of color.
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
    /// `-` dim: planned removal in a consent plan; flips to `Done`/`Warn` in the
    /// receipt.
    Plan,
    /// `=` dim: planned keep in a consent plan, the counterpart to `Plan`.
    Keep,
}

impl Glyph {
    /// The bare glyph, one visible column, no color.
    pub(crate) const fn symbol(self) -> &'static str {
        match self {
            Self::Done => "✓",
            Self::Warn => "!",
            Self::Fail => "✗",
            Self::Skip => "•",
            Self::Plan => "-",
            Self::Keep => "=",
        }
    }

    /// The glyph colored by its role. Only the glyph carries ANSI so a caller can
    /// pad the plain key beside it without disturbing column math.
    pub(crate) fn render(self, mode: impl Into<ColorMode>) -> String {
        let mode = mode.into();
        match self {
            Self::Done => success(self.symbol(), mode),
            Self::Warn => warn(self.symbol(), mode),
            Self::Fail => error(self.symbol(), mode),
            Self::Skip | Self::Plan | Self::Keep => dim(self.symbol(), mode),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_is_one_visible_column() {
        for glyph in [
            Glyph::Done,
            Glyph::Warn,
            Glyph::Fail,
            Glyph::Skip,
            Glyph::Plan,
            Glyph::Keep,
        ] {
            assert_eq!(glyph.symbol().chars().count(), 1, "{glyph:?}");
        }
    }

    #[test]
    fn forced_color_mode_never_touches_the_real_process_streams() {
        // `ColorMode::Forced` bypasses `color_enabled` entirely, so a role
        // function driven by an injected bool can be asserted deterministically
        // regardless of whether this test binary's stdout/stderr are TTYs.
        assert_eq!(success("x", true), format!("{}", "x".green()));
        assert_eq!(success("x", false), "x");
        assert_eq!(dim("y", true), format!("{}", "y".dimmed()));
        assert_eq!(dim("y", false), "y");
    }

    #[test]
    fn accentuate_strips_backticks_and_colors_only_with_forced_mode() {
        assert_eq!(
            accentuate("run `omnifs up` now", true),
            format!("run {} now", accent("omnifs up", true))
        );
        assert_eq!(
            accentuate("run `omnifs up` now", false),
            "run omnifs up now"
        );
        // Unbalanced backticks are left literal.
        assert_eq!(accentuate("a `b", false), "a `b");
    }
}
