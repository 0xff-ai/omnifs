//! The flat human renderer: ledger blocks, sentences, headings, hints, and the
//! error block. This is the one place that turns typed report content into
//! bytes for the CLI experience v2 register (spec 2.1): a left-anchored
//! stream with no frames, no gutters, and no repetition of the command the
//! user just typed.
//!
//! All ANSI-width math for that register lives here. Callers inject terminal
//! [`Capabilities`] rather than this module probing a real TTY, so every
//! primitive is deterministic under test; `table.rs` established the same
//! `render_with`-style capability injection for the responsive report and
//! this module follows it.
//!
//! `Output::outro`/`plan`/`receipt` (`ui/output.rs`) construct a real
//! `Capabilities` from the process and call `sentence`/`heading`; the rest of
//! this module (`ledger_block`, `hint`, `error_block`, ...) is still
//! exercised only by its own unit tests until a later slice in the CLI
//! experience v2 campaign migrates full command reports onto it.
#![allow(dead_code)]

use super::style::{self, Glyph};

/// Injected terminal capabilities for one render call. Real detection (an
/// `is_tty` probe, `crossterm::terminal::size`, `style::color_enabled`) is a
/// caller concern, added when a command migrates onto this renderer; every
/// primitive here only ever reads the values it was given.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Capabilities {
    pub(crate) width: usize,
    pub(crate) is_tty: bool,
    pub(crate) color: bool,
    pub(crate) quiet: bool,
}

/// Measure a string's terminal column width, ignoring SGR escapes and
/// counting wide glyphs (CJK, emoji) as two columns. Shared by every
/// alignment computation in this module so key columns, wrapping, and the
/// error block agree on what "one column" means.
fn display_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthChar as _;
    super::strip_ansi(text)
        .chars()
        .map(|ch| ch.width().unwrap_or(0))
        .sum()
}

/// Greedy word-wrap to `width` columns. `width == 0` disables wrapping
/// (returns the text as one line) rather than producing an infinite column of
/// single characters.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_owned()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let extra = usize::from(!current.is_empty());
        let candidate = display_width(&current) + extra + display_width(word);
        if candidate > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// One row of a ledger block: `<glyph> <key>` followed by the value, key
/// column sized to the block it belongs to (spec 2.1).
#[derive(Debug, Clone)]
pub(crate) struct LedgerRow {
    pub(crate) glyph: Glyph,
    pub(crate) key: String,
    pub(crate) value: String,
}

impl LedgerRow {
    pub(crate) fn new(glyph: Glyph, key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            glyph,
            key: key.into(),
            value: value.into(),
        }
    }
}

/// The fixed gap after the widest key in a block. Derived from the spec 2.1
/// worked example (`providers` at 9 columns leaves a 3-space gap, `daemon` at
/// 6 columns leaves a 6-space gap: both resolve to a 12-column key field, i.e.
/// `max_key_width + 3`).
const LEDGER_GAP: usize = 3;

/// Render one contiguous ledger block. Left-anchored, no leading indent: the
/// glyph starts at column 0. The key column is sized to the longest key in
/// `rows` alone, never truncated and never fixed across blocks, so a report
/// with several blocks can have a different key width per block.
pub(crate) fn ledger_block(rows: &[LedgerRow], caps: Capabilities) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let key_width = rows
        .iter()
        .map(|row| display_width(&row.key))
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|row| {
            let pad = key_width - display_width(&row.key) + LEDGER_GAP;
            format!(
                "{} {}{}{}",
                row.glyph.render(caps.color),
                row.key,
                " ".repeat(pad),
                style::accentuate(&row.value, caps.color)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A plain sentence-case line, wrapped to `caps.width`. Inline command spans
/// (`` `cmd` ``) accent-color and drop their backticks per line, so a wrap
/// point can never leave a dangling escape sequence.
pub(crate) fn sentence(text: &str, caps: Capabilities) -> String {
    wrap(text, caps.width)
        .iter()
        .map(|line| style::accentuate(line, caps.color))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A bold section heading word (`Frontends`, `Mounts`). Plain bold, not the
/// blue heading role: spec 2.4 prefers plain bold for report sections.
pub(crate) fn heading(text: &str, caps: Capabilities) -> String {
    style::bold(text, caps.color)
}

/// The single most-actionable next step: a command in the accent color plus
/// a dim description. `--quiet` suppresses it, since it exists to prompt a
/// human, not to carry information a script needs.
pub(crate) fn hint(cmd: &str, desc: &str, caps: Capabilities) -> Option<String> {
    if caps.quiet {
        return None;
    }
    Some(format!(
        "{}  {}",
        style::accent(cmd, caps.color),
        style::dim(desc, caps.color)
    ))
}

/// The indented "Last daemon log lines:"-style detail under an error
/// headline.
#[derive(Debug, Clone)]
pub(crate) struct ErrorDetail {
    pub(crate) heading: String,
    pub(crate) lines: Vec<String>,
}

/// One `Fix:`/`Log:`/`Try:` recovery line.
#[derive(Debug, Clone)]
pub(crate) struct ErrorAction {
    pub(crate) label: &'static str,
    pub(crate) value: String,
}

impl ErrorAction {
    pub(crate) fn fix(command: impl Into<String>) -> Self {
        Self {
            label: "Fix",
            value: command.into(),
        }
    }

    pub(crate) fn log(path: impl Into<String>) -> Self {
        Self {
            label: "Log",
            value: path.into(),
        }
    }

    pub(crate) fn try_(value: impl Into<String>) -> Self {
        Self {
            label: "Try",
            value: value.into(),
        }
    }
}

/// The error block shape from spec 2.8. Wiring a live error into this shape
/// is slice S5's job; this module only owns the render primitive.
#[derive(Debug, Clone)]
pub(crate) struct ErrorBlock {
    pub(crate) headline: String,
    pub(crate) detail: Option<ErrorDetail>,
    pub(crate) actions: Vec<ErrorAction>,
    pub(crate) id: Option<String>,
}

/// Strip an embedded `(id: <slug>)` trailer from a pre-rendered nested error
/// message. A cause folded into an [`ErrorDetail`] may already carry the
/// trailer from when it was rendered standalone; the outer block adds its own
/// trailer once at the end, so a nested one would duplicate it.
pub(crate) fn strip_id_trailer(text: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.ends_with(')')
        && let Some(start) = trimmed.rfind("(id: ")
    {
        return trimmed[..start].trim_end().to_owned();
    }
    text.to_owned()
}

pub(crate) fn error_block(block: &ErrorBlock, caps: Capabilities) -> String {
    let mut out = String::new();
    out.push_str(&Glyph::Fail.render(caps.color));
    out.push(' ');
    out.push_str(&sentence(&block.headline, caps));
    out.push('\n');

    if let Some(detail) = &block.detail {
        out.push('\n');
        out.push_str("  ");
        out.push_str(&detail.heading);
        out.push('\n');
        for line in &detail.lines {
            out.push_str("    ");
            out.push_str(&strip_id_trailer(line));
            out.push('\n');
        }
    }

    if !block.actions.is_empty() {
        out.push('\n');
        for action in &block.actions {
            out.push_str(action.label);
            out.push_str(":  ");
            out.push_str(&style::accent(&action.value, caps.color));
            out.push('\n');
        }
    }

    if let Some(id) = &block.id {
        out.push('\n');
        out.push_str(&style::dim(format!("(id: {id})"), caps.color));
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(width: usize, color: bool) -> Capabilities {
        Capabilities {
            width,
            is_tty: color,
            color,
            quiet: false,
        }
    }

    #[test]
    fn ledger_block_sizes_the_key_column_per_block() {
        let rows = [
            LedgerRow::new(Glyph::Done, "providers", "2/2 warm (1.2s)"),
            LedgerRow::new(
                Glyph::Done,
                "daemon",
                "running (pid 31114), revision 3f69473",
            ),
        ];
        let rendered = ledger_block(&rows, caps(120, false));
        assert_eq!(
            rendered,
            "✓ providers   2/2 warm (1.2s)\n✓ daemon      running (pid 31114), revision 3f69473"
        );
    }

    #[test]
    fn ledger_block_key_width_resets_between_blocks() {
        // A block with a short longest key gets a narrower key column than a
        // block with a long one: sizing is per block, never fixed crate-wide.
        let short_block = [LedgerRow::new(Glyph::Done, "ok", "value")];
        let long_block = [LedgerRow::new(Glyph::Done, "a much longer key", "value")];
        let short = ledger_block(&short_block, caps(120, false));
        let long = ledger_block(&long_block, caps(120, false));
        assert_eq!(short, "✓ ok   value");
        assert_eq!(long, "✓ a much longer key   value");
    }

    #[test]
    fn ledger_block_colors_the_glyph_and_accentuates_the_value() {
        let rows = [LedgerRow::new(Glyph::Fail, "daemon", "crashed `logs`")];
        let rendered = ledger_block(&rows, caps(120, true));
        assert!(
            rendered.starts_with(&format!("{} daemon", Glyph::Fail.render(true))),
            "{rendered:?}"
        );
        assert!(
            rendered.contains(&style::accent("logs", true)),
            "{rendered:?}"
        );
        assert!(!rendered.contains('`'), "{rendered:?}");
    }

    #[test]
    fn keep_glyph_renders_as_equals_and_stays_dim() {
        let rows = [LedgerRow::new(Glyph::Keep, "credential store", "kept")];
        let plain = ledger_block(&rows, caps(120, false));
        assert!(plain.starts_with("= credential store"), "{plain:?}");
        let colored = ledger_block(&rows, caps(120, true));
        assert!(
            colored.starts_with(&Glyph::Keep.render(true)),
            "{colored:?}"
        );
    }

    #[test]
    fn sentence_wraps_to_width_and_ends_with_a_period() {
        let text = "The daemon exited before your mounts came ready.";
        let rendered = sentence(text, caps(20, false));
        for line in rendered.lines() {
            assert!(display_width(line) <= 20, "{line:?}");
        }
        assert!(rendered.ends_with('.'));
        assert_eq!(sentence(text, caps(120, false)), text);
    }

    #[test]
    fn sentence_accentuates_backticks_per_color_mode() {
        let text = "Run `omnifs up` to start.";
        assert_eq!(
            sentence(text, caps(120, true)),
            format!("Run {} to start.", style::accent("omnifs up", true))
        );
        assert_eq!(sentence(text, caps(120, false)), "Run omnifs up to start.");
    }

    #[test]
    fn heading_is_bold_only_when_color_is_on() {
        assert_eq!(
            heading("Frontends", caps(120, true)),
            style::bold("Frontends", true)
        );
        assert_eq!(heading("Frontends", caps(120, false)), "Frontends");
    }

    #[test]
    fn hint_accents_the_command_and_quiet_suppresses_it() {
        let line = hint(
            "omnifs mount add github",
            "connect a provider",
            caps(120, false),
        )
        .expect("hint renders when not quiet");
        assert_eq!(line, "omnifs mount add github  connect a provider");

        let mut quiet = caps(120, false);
        quiet.quiet = true;
        assert!(hint("omnifs up", "start the daemon", quiet).is_none());
    }

    #[test]
    fn error_block_matches_the_documented_shape() {
        let block = ErrorBlock {
            headline: "The daemon exited before your mounts came ready.".to_owned(),
            detail: Some(ErrorDetail {
                heading: "Last daemon log lines:".to_owned(),
                lines: vec!["ERROR provider github: pinned artifact missing from store".to_owned()],
            }),
            actions: vec![
                ErrorAction::fix("omnifs mount add github"),
                ErrorAction::log("~/.omnifs/cache/daemon.log"),
            ],
            id: Some("daemon-unreachable".to_owned()),
        };
        let rendered = error_block(&block, caps(120, false));
        assert_eq!(
            rendered,
            "✗ The daemon exited before your mounts came ready.\n\
             \n\
             \x20\x20Last daemon log lines:\n\
             \x20\x20\x20\x20ERROR provider github: pinned artifact missing from store\n\
             \n\
             Fix:  omnifs mount add github\n\
             Log:  ~/.omnifs/cache/daemon.log\n\
             \n\
             (id: daemon-unreachable)\n"
        );
    }

    #[test]
    fn error_block_strips_a_duplicate_id_trailer_from_detail_lines() {
        assert_eq!(
            strip_id_trailer("pinned artifact missing (id: mount-degraded)"),
            "pinned artifact missing"
        );
        assert_eq!(strip_id_trailer("no trailer here"), "no trailer here");

        let block = ErrorBlock {
            headline: "Mount degraded.".to_owned(),
            detail: Some(ErrorDetail {
                heading: "Caused by:".to_owned(),
                lines: vec!["upstream failed (id: mount-degraded)".to_owned()],
            }),
            actions: Vec::new(),
            id: Some("mount-degraded".to_owned()),
        };
        let rendered = error_block(&block, caps(120, false));
        assert_eq!(rendered.matches("(id: mount-degraded)").count(), 1);
    }

    #[test]
    fn piped_capabilities_are_deterministic_without_a_real_tty() {
        // The stable piped shape (120 columns, no ANSI) is a caller-supplied
        // capability here, not something this module probes for itself.
        let piped = Capabilities {
            width: 120,
            is_tty: false,
            color: false,
            quiet: false,
        };
        let rows = [LedgerRow::new(Glyph::Done, "mounts", "3 attached")];
        let rendered = ledger_block(&rows, piped);
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert_eq!(rendered, "✓ mounts   3 attached");
    }
}
