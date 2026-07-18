//! The flat human renderer: ledger blocks, sentences, headings, and the
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
use std::io::IsTerminal as _;

use super::style::{self, Glyph};

/// Real stdout terminal capabilities, mirroring `output.rs`'s stderr
/// equivalent (`stderr_capabilities`) for human-mode command output that
/// prints its record to stdout rather than narrating to stderr.
pub(crate) fn stdout_capabilities() -> Capabilities {
    let is_tty = std::io::stdout().is_terminal();
    Capabilities {
        width: if is_tty {
            crossterm::terminal::size().map_or(80, |(columns, _rows)| usize::from(columns))
        } else {
            120
        },
        is_tty,
        color: style::color_enabled(style::Stream::Stdout),
        quiet: false,
    }
}

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
/// error block agree on what "one column" means. `pub(crate)` so callers that
/// render their own transient frame outside this module (`ui/live.rs`'s
/// spinner) still measure keys the same way a settled ledger row does.
pub(crate) fn display_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthChar as _;
    super::strip_ansi(text)
        .chars()
        .map(|ch| ch.width().unwrap_or(0))
        .sum()
}

/// The terminal's current column width, sampled fresh on every call rather
/// than cached. Callers here are raw-mode frame drawers (`ui/prompt.rs`,
/// `ui/live.rs`) that redraw repeatedly over one interactive session, and a
/// mid-session resize must be picked up by the very next frame instead of
/// silently wrapping against a stale width. `80` is the same conservative
/// fallback `stdout_capabilities`/`stderr_capabilities` fall back to when
/// `crossterm::terminal::size` itself errors; unlike those two, this probe
/// never substitutes the wider piped-output default, since every caller here
/// has already confirmed a live TTY (raw mode requires one to enter at all).
pub(crate) fn terminal_width() -> usize {
    crossterm::terminal::size().map_or(80, |(columns, _rows)| usize::from(columns))
}

/// The number of physical terminal rows one logical `line` occupies once the
/// terminal wraps it: `ceil(display_width / width)`. `width == 0` (no
/// terminal size available) disables wrapping, so the line always occupies
/// exactly one row; an empty line also occupies exactly one row rather than
/// zero, since a drawn blank line still consumes a row on screen. A line
/// whose width is an exact multiple of `width` fills those rows completely
/// and never spills an extra blank row underneath: a terminal only wraps
/// once content actually exceeds the column, never on a line that lands
/// exactly at the edge.
///
/// The one owner of this math: every raw-mode frame drawer that tracks drawn
/// rows (`ui/prompt.rs`'s `redraw`/`erase`, `ui/live.rs`'s
/// `LiveRegion::draw`/`erase` and `Spinner`) computes through this function
/// instead of re-deriving the ceiling division locally, so a `MoveUp` always
/// targets the row a previous frame actually wrapped onto.
pub(crate) fn physical_rows(line: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    display_width(line).div_ceil(width).max(1)
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
/// `max_key_width + 3`). `pub(crate)` so `ui/live.rs`'s transient spinner
/// frame (which never sees a full `LedgerRow` slice, only a bare key) can
/// share the exact same pad math as [`ledger_row_line`] instead of
/// duplicating the constant.
pub(crate) const LEDGER_GAP: usize = 3;

/// The key width a block of `rows` needs so every row's value column lines
/// up. Callers that print rows one at a time as async work settles (rather
/// than as one batch) compute this once from their known key set and pass it
/// to [`ledger_row_line`] for every row, so the block still reads as one
/// aligned unit even though no single call ever sees every row at once.
pub(crate) fn ledger_key_width(rows: &[LedgerRow]) -> usize {
    rows.iter()
        .map(|row| display_width(&row.key))
        .max()
        .unwrap_or(0)
}

/// The same block-sizing math as [`ledger_key_width`], but from bare key
/// text rather than fully-formed rows: a flow that emits its rows one at a
/// time as async work settles (spinner settle, `Output::ledger_row`) knows
/// every key it might ever print before the first one lands, but not the
/// values, so it cannot build a `[LedgerRow]` slice up front. Declaring the
/// key set once and sizing from it here is what keeps such a block aligned
/// even though no single call ever sees every row at once; a key that ends
/// up not emitted still counts toward the width.
pub(crate) fn key_field_width(keys: &[&str]) -> usize {
    keys.iter().copied().map(display_width).max().unwrap_or(0)
}

/// A counted noun that agrees in number (`1 mount`, `3 mounts`, `0
/// credentials`): the shared pluralization the human register uses instead
/// of a parenthetical `(s)`, which the style register forbids outright.
pub(crate) fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Render one ledger row against an externally supplied key width. Never
/// truncates: a key wider than `key_width` still gets its `LEDGER_GAP`
/// separation before the value, it just breaks alignment with its
/// neighbors rather than losing characters.
pub(crate) fn ledger_row_line(row: &LedgerRow, key_width: usize, caps: Capabilities) -> String {
    let pad = key_width.saturating_sub(display_width(&row.key)) + LEDGER_GAP;
    format!(
        "{} {}{}{}",
        row.glyph.render(caps.color),
        row.key,
        " ".repeat(pad),
        style::accentuate(&row.value, caps.color)
    )
}

/// The column where a ledger row's value begins, counting from the row's own
/// left edge (glyph column 0). Shared by [`ledger_row_line`] and any caller
/// that must align a continuation line under a row's value without
/// duplicating the gap math: doctor's per-row `fix:` line (spec 3.9) is one
/// such caller.
pub(crate) fn ledger_value_column(key_width: usize) -> usize {
    // Glyph (1) + one space (1) precede the key, then the key's own field.
    key_width + LEDGER_GAP + 2
}

/// Render one contiguous ledger block. Left-anchored, no leading indent: the
/// glyph starts at column 0. The key column is sized to the longest key in
/// `rows` alone, never truncated and never fixed across blocks, so a report
/// with several blocks can have a different key width per block.
pub(crate) fn ledger_block(rows: &[LedgerRow], caps: Capabilities) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let key_width = ledger_key_width(rows);
    rows.iter()
        .map(|row| ledger_row_line(row, key_width, caps))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The consent plan preview block (spec 2.7): a headline sentence naming the
/// operation, then its rows indented two spaces under it with the `-`/`=`
/// glyph vocabulary (`rows` is expected to carry only [`Glyph::Plan`] and
/// [`Glyph::Keep`] rows; the receipt that settles a plan uses
/// [`ledger_block`] directly, unindented, since the operation already
/// happened by then). Pure, so `Output::plan`'s exact transcript is testable
/// without capturing real stderr.
pub(crate) fn plan_block(title: &str, rows: &[LedgerRow], caps: Capabilities) -> String {
    let mut out = sentence(title, caps);
    out.push('\n');
    for line in ledger_block(rows, caps).lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out
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
    fn streamed_ledger_rows_match_a_batch_ledger_block_of_the_same_rows() {
        // Rows printed one at a time as async work settles (up/down's real
        // usage) must read identically to the same rows rendered as one
        // batch: the shared key width is the only thing that has to be
        // computed up front.
        let rows = [
            LedgerRow::new(
                Glyph::Done,
                "daemon",
                "running (pid 31114), revision 3f69473",
            ),
            LedgerRow::new(Glyph::Done, "mounts", "/github /dns serving"),
        ];
        let batch = ledger_block(&rows, caps(120, false));
        let width = ledger_key_width(&rows);
        let streamed = rows
            .iter()
            .map(|row| ledger_row_line(row, width, caps(120, false)))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(batch, streamed);
    }

    #[test]
    fn ledger_row_line_never_truncates_a_key_wider_than_the_shared_width() {
        let row = LedgerRow::new(Glyph::Done, "credential store", "kept");
        let rendered = ledger_row_line(&row, 6, caps(120, false));
        assert!(rendered.contains("credential store"), "{rendered:?}");
        assert!(!rendered.contains('…'), "{rendered:?}");
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
    fn plan_block_matches_spec_2_7s_documented_shape() {
        // The exact 2.7 illustrative transcript: a headline, then three
        // indented rows mixing the `-` (removal) and `=` (keep) glyphs. No
        // current command reaches a three-row plan (`mount rm`/`mount revoke`
        // each only ever plan one row today), so this exercises the
        // primitive directly against the documented shape rather than
        // through a live command.
        let rows = [
            LedgerRow::new(Glyph::Plan, "mount", "/github"),
            LedgerRow::new(Glyph::Plan, "credential", "github oauth, revoked upstream"),
            LedgerRow::new(Glyph::Keep, "provider", "artifact kept in store"),
        ];
        let rendered = plan_block("Removing mount github", &rows, caps(120, false));
        let lines = rendered.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "Removing mount github");
        assert_eq!(lines.len(), 4, "{rendered:?}");
        // Every row is indented two spaces under the headline, and the
        // indented block is byte-for-byte the same rows `ledger_block` alone
        // would produce (plan_block only adds the headline and the indent).
        let unindented = ledger_block(&rows, caps(120, false));
        for (line, expected) in lines[1..].iter().zip(unindented.lines()) {
            assert_eq!(*line, format!("  {expected}"));
        }
        assert!(lines[1].trim_start().starts_with("- mount"), "{rendered:?}");
        assert!(
            lines[2].trim_start().starts_with("- credential"),
            "{rendered:?}"
        );
        assert!(
            lines[3].trim_start().starts_with("= provider"),
            "{rendered:?}"
        );
        // Every row's value starts at the same column: the plan reads as one
        // aligned block, not three independently-sized rows.
        let columns = [
            lines[1].find("/github").unwrap(),
            lines[2].find("github oauth").unwrap(),
            lines[3].find("artifact").unwrap(),
        ];
        assert_eq!(columns[0], columns[1], "{rendered:?}");
        assert_eq!(columns[1], columns[2], "{rendered:?}");
    }

    #[test]
    fn plan_block_is_empty_bodied_for_a_plan_with_no_rows() {
        assert_eq!(
            plan_block("Removing mount x", &[], caps(120, false)),
            "Removing mount x\n"
        );
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
    fn ledger_value_column_matches_where_ledger_row_line_actually_starts_the_value() {
        let row = LedgerRow::new(Glyph::Warn, "credentials", "github token expired");
        let key_width = ledger_key_width(std::slice::from_ref(&row));
        let rendered = ledger_row_line(&row, key_width, caps(120, false));
        let value_start = rendered.find("github").expect("value present");
        assert_eq!(value_start, ledger_value_column(key_width));
    }

    #[test]
    fn key_field_width_matches_ledger_key_width_for_the_same_keys() {
        let keys = ["providers", "daemon", "mounts", "frontends"];
        let rows = keys
            .iter()
            .map(|key| LedgerRow::new(Glyph::Done, (*key).to_owned(), String::new()))
            .collect::<Vec<_>>();
        assert_eq!(key_field_width(&keys), ledger_key_width(&rows));
        assert_eq!(
            key_field_width(&keys),
            9,
            "`providers`/`frontends` tie at 9"
        );
    }

    #[test]
    fn count_agrees_in_number_and_never_uses_a_parenthetical_plural() {
        assert_eq!(count(1, "credential"), "1 credential");
        assert_eq!(count(0, "credential"), "0 credentials");
        assert_eq!(count(3, "credential"), "3 credentials");
        for text in [count(0, "mount"), count(1, "mount"), count(2, "mount")] {
            assert!(!text.contains("(s)"), "{text:?}");
        }
    }

    // -- physical_rows: the one owner of wrap-aware row counting ------------

    #[test]
    fn physical_rows_is_one_for_a_line_exactly_at_the_width() {
        assert_eq!(physical_rows(&"x".repeat(80), 80), 1);
    }

    #[test]
    fn physical_rows_wraps_a_line_one_column_over_the_width() {
        assert_eq!(physical_rows(&"x".repeat(81), 80), 2);
    }

    #[test]
    fn physical_rows_measures_display_width_not_byte_length_through_ansi() {
        // An 81-column line still wraps to 2 rows once colored, because the
        // ANSI escapes it gained are stripped before measuring, the same way
        // `display_width` already treats them as zero-width.
        let colored = style::accent("x".repeat(81), true);
        assert_eq!(physical_rows(&colored, 80), 2);
    }

    #[test]
    fn physical_rows_never_wraps_when_the_terminal_width_is_unknown() {
        assert_eq!(physical_rows(&"x".repeat(500), 0), 1);
    }

    #[test]
    fn physical_rows_is_one_for_an_empty_line() {
        assert_eq!(physical_rows("", 80), 1);
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
