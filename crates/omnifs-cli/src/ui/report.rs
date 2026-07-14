//! Rows used by the live output and consent rails.

// This module is the sanctioned rail output owner.
#![allow(clippy::disallowed_macros, clippy::print_stdout)]

use super::style::{self, Glyph};

const LEDGER_KEY_WIDTH: usize = 14;

/// A single fixed-width rail row: `  <glyph> <key padded>value`.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub(crate) glyph: Glyph,
    pub(crate) key: String,
    pub(crate) value: String,
}

impl Row {
    pub(crate) fn new(glyph: Glyph, key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            glyph,
            key: key.into(),
            value: value.into(),
        }
    }

    /// Render the row with a stable key column. Inline command spans are
    /// accentuated by the rail style owner.
    pub(super) fn render(&self) -> String {
        let display_key = super::truncate(&self.key, LEDGER_KEY_WIDTH);
        let key_pad = (LEDGER_KEY_WIDTH - display_key.chars().count()).max(1);
        format!(
            "  {} {}{}{}",
            self.glyph.render(),
            display_key,
            " ".repeat(key_pad),
            style::accentuate(&self.value)
        )
    }
}

/// Render a group of rail rows as one borderless table so the key and value
/// columns size to the complete operation, rather than truncating each key at
/// a fixed width.
pub(super) fn render_rows(rows: &[Row]) -> String {
    use tabled::builder::Builder;
    use tabled::settings::{Padding, Style};

    if rows.is_empty() {
        return String::new();
    }

    let mut builder = Builder::default();
    for row in rows {
        builder.push_record([
            row.glyph.render(),
            row.key.clone(),
            style::accentuate(&row.value),
        ]);
    }

    let mut table = builder.build();
    table.with(Style::empty()).with(Padding::new(0, 1, 0, 0));
    table
        .to_string()
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::strip_ansi;

    #[test]
    fn rows_keep_a_separator_before_the_value() {
        let row = Row::new(Glyph::Done, "frontends observed", "2 attached");
        let plain = strip_ansi(&row.render());
        assert!(plain.contains("frontends ob… 2 attached"), "{plain:?}");
    }

    #[test]
    fn grouped_rows_keep_full_keys_and_align_values() {
        let rows = [
            Row::new(
                Glyph::Plan,
                "frontend nfs (host)",
                "tear down /Users/raul/omnifs",
            ),
            Row::new(Glyph::Plan, "frontend fuse (docker)", "tear down /omnifs"),
            Row::new(Glyph::Plan, "daemon", "stop if running"),
        ];
        let plain = strip_ansi(&render_rows(&rows));
        assert!(plain.contains("frontend nfs (host)"), "{plain:?}");
        assert!(plain.contains("frontend fuse (docker)"), "{plain:?}");
        let nfs = plain
            .lines()
            .find(|line| line.contains("frontend nfs"))
            .unwrap();
        let fuse = plain
            .lines()
            .find(|line| line.contains("frontend fuse"))
            .unwrap();
        assert_eq!(nfs.find("tear down"), fuse.find("tear down"), "{plain:?}");
    }
}
