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
    pub(crate) fn render(&self, mode: impl Into<style::ColorMode>) -> String {
        let mode = mode.into();
        let display_key = super::truncate(&self.key, LEDGER_KEY_WIDTH);
        let key_pad = (LEDGER_KEY_WIDTH - display_key.chars().count()).max(1);
        format!(
            "  {} {}{}{}",
            self.glyph.render(mode),
            display_key,
            " ".repeat(key_pad),
            style::accentuate(&self.value, mode)
        )
    }
}

/// Render a group of rail rows as one borderless block so the key and value
/// columns size to the complete operation, rather than truncating each key at
/// a fixed width. `mode` is copied per row, not per call, so every row in the
/// block agrees on whether color is on.
pub(super) fn render_rows(rows: &[Row], mode: impl Into<style::ColorMode> + Copy) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let key_width = rows
        .iter()
        .map(|row| row.key.chars().count())
        .max()
        .unwrap_or(0);
    rows.iter()
        .map(|row| {
            let key_pad = key_width - row.key.chars().count();
            format!(
                "{} {}{} {}",
                row.glyph.render(mode),
                row.key,
                " ".repeat(key_pad),
                style::accentuate(&row.value, mode)
            )
            .trim_end()
            .to_owned()
        })
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
        let plain = strip_ansi(&row.render(false));
        assert!(plain.contains("frontends obs… 2 attached"), "{plain:?}");
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
        let plain = strip_ansi(&render_rows(&rows, false));
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
