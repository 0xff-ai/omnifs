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
}
