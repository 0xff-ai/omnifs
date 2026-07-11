//! Typed state reports: the workhorse for `omnifs status`, `doctor`, and the
//! mount list.
//!
//! A command builds a [`Report`] of titled [`Section`]s, each a run of
//! [`Row`]s (glyph, key, value, optional `fix` command). One render path draws
//! the flat human grid to stdout; one `Serialize` path emits the same rows as
//! JSON, so `--json` cannot drift from the human view. Stream discipline is not
//! the command's problem: reports go to stdout, and only this module prints
//! them.

// This module is the sanctioned output owner; the drift gate denies print
// macros everywhere else.
#![allow(clippy::disallowed_macros, clippy::print_stdout)]

use std::fmt::Write as _;

use serde::Serialize;
use serde::ser::{SerializeStruct as _, Serializer};

use super::KEY_WIDTH;
use super::style::{self, Glyph};

/// One report row on the shared grid: `  <glyph> <key>value`. The optional
/// `fix` carries the next command; the human grid already shows it inside the
/// value column, so `fix` exists to expose that command as a discrete JSON
/// field (the doctor pattern, generalized).
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub(crate) glyph: Glyph,
    pub(crate) key: String,
    pub(crate) value: String,
    pub(crate) fix: Option<String>,
    /// Render the key bold: true for identity keys (mount, provider, frontend
    /// names), false for machinery labels (daemon, home).
    pub(crate) bold_key: bool,
}

impl Row {
    pub(crate) fn new(glyph: Glyph, key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            glyph,
            key: key.into(),
            value: value.into(),
            fix: None,
            bold_key: false,
        }
    }

    /// Mark the key as an identity name so it renders bold.
    pub(crate) fn identity(mut self) -> Self {
        self.bold_key = true;
        self
    }

    /// Attach the next command for this row; the value column already contains
    /// it, this makes it machine-visible.
    pub(crate) fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }

    /// A single fixed-width row for the rail (`  <glyph> <key padded>value`).
    /// The flat [`Report`] no longer routes through here: it builds a `tabled`
    /// grid so columns size to content. This stays for the rail, whose keys are
    /// short; the value renders `` `cmd` `` spans as the cyan accent.
    pub(super) fn render(&self) -> String {
        let display_key = super::truncate(&self.key, KEY_WIDTH);
        // Always keep at least one space before the value so a key that fills
        // the column never jams against it.
        let key_pad = (KEY_WIDTH - display_key.chars().count()).max(1);
        let key = if self.bold_key {
            style::bold(&display_key)
        } else {
            display_key
        };
        format!(
            "  {} {key}{:pad$}{}",
            self.glyph.render(),
            "",
            style::accentuate(&self.value),
            pad = key_pad
        )
    }
}

impl Serialize for Row {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut row = serializer.serialize_struct("Row", 4)?;
        row.serialize_field("state", self.glyph.json_state())?;
        row.serialize_field("key", &self.key)?;
        row.serialize_field("value", &self.value)?;
        row.serialize_field("fix", &self.fix)?;
        row.end()
    }
}

/// A titled block of rows. `count`, when set, renders as `Title (n)`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Section {
    pub(crate) title: String,
    pub(crate) count: Option<usize>,
    pub(crate) rows: Vec<Row>,
}

impl Section {
    pub(crate) fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            count: None,
            rows: Vec::new(),
        }
    }

    /// Set the parenthetical count that follows the heading (`Mounts (3)`).
    pub(crate) fn counted(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    pub(crate) fn push(&mut self, row: Row) {
        self.rows.push(row);
    }

    fn heading(&self) -> String {
        match self.count {
            Some(count) => style::heading(format!("{} ({count})", self.title)),
            None => style::heading(&self.title),
        }
    }
}

/// A whole command's human + machine output. Sections render top to bottom with
/// a blank line between them and no horizontal rules.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    pub(crate) sections: Vec<Section>,
}

impl Report {
    pub(crate) fn new() -> Self {
        Self {
            sections: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, section: Section) {
        self.sections.push(section);
    }

    /// The flat human grid: a colored bold heading per section, then a
    /// borderless `tabled` grid whose glyph/key/value columns size to content
    /// (unicode- and ANSI-width aware), a blank line between sections, no rules.
    pub(crate) fn render(&self) -> String {
        use tabled::builder::Builder;
        use tabled::settings::object::Columns;
        use tabled::settings::{Padding, Style};

        let mut out = String::new();
        for (index, section) in self.sections.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            let _ = writeln!(out, "{}", section.heading());
            if section.rows.is_empty() {
                continue;
            }
            let mut builder = Builder::default();
            for row in &section.rows {
                let key = if row.bold_key {
                    style::bold(&row.key)
                } else {
                    row.key.clone()
                };
                builder.push_record([row.glyph.render(), key, style::accentuate(&row.value)]);
            }
            let mut table = builder.build();
            // Borderless: one space between columns (right padding), a two-space
            // gutter before the glyph column. Style::empty drops every rule.
            table
                .with(Style::empty())
                .with(Padding::new(0, 1, 0, 0))
                .modify(Columns::first(), Padding::new(2, 1, 0, 0));
            // tabled pads every cell to its column width, so the last column
            // trails spaces; strip them so lines end at their content.
            for line in table.to_string().lines() {
                let _ = writeln!(out, "{}", line.trim_end());
            }
        }
        out
    }

    /// Print the human grid to stdout. Reports are documents, so they go to
    /// stdout while narration goes to stderr.
    pub(crate) fn print(&self) {
        anstream::print!("{}", self.render());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::strip_ansi;

    fn sample() -> Report {
        let mut report = Report::new();
        let mut head = Section::new("omnifs 0.2.1");
        head.push(Row::new(Glyph::Done, "daemon", "running (pid 41231)"));
        head.push(Row::new(Glyph::Skip, "home", "~/.omnifs"));
        report.push(head);
        let mut mounts = Section::new("Mounts").counted(2);
        mounts.push(Row::new(Glyph::LiveDot, "github", "oauth, 2 scopes").identity());
        mounts.push(
            Row::new(Glyph::Warn, "linear", "credential expired; run `x`")
                .identity()
                .with_fix("omnifs mount reauth linear"),
        );
        report.push(mounts);
        report
    }

    #[test]
    fn rows_within_a_section_align_their_values() {
        let plain = strip_ansi(&sample().render());
        let daemon = plain.lines().find(|l| l.contains("daemon")).unwrap();
        let home = plain.lines().find(|l| l.contains("home")).unwrap();
        // The grid aligns every value in a section to one column, and a space
        // always separates the key from the value (no jamming).
        assert_eq!(
            daemon.find("running"),
            home.find("~/.omnifs"),
            "values misaligned:\n{daemon:?}\n{home:?}"
        );
        assert!(daemon.contains("daemon "), "key jams value: {daemon:?}");
    }

    #[test]
    fn sections_get_a_blank_line_between_them() {
        let plain = strip_ansi(&sample().render());
        assert!(plain.contains("omnifs 0.2.1\n"));
        assert!(plain.contains("Mounts (2)\n"));
        // A blank line separates the two sections.
        assert!(plain.contains("\n\nMounts (2)\n"), "{plain:?}");
    }

    #[test]
    fn json_carries_state_and_fix() {
        let json = serde_json::to_value(sample()).unwrap();
        let mounts = &json["sections"][1];
        assert_eq!(mounts["title"], "Mounts");
        assert_eq!(mounts["count"], 2);
        assert_eq!(mounts["rows"][0]["state"], "ok");
        assert_eq!(mounts["rows"][1]["state"], "warn");
        assert_eq!(mounts["rows"][1]["fix"], "omnifs mount reauth linear");
    }

    #[test]
    fn long_keys_truncate_and_keep_a_separator_in_the_rail() {
        // `Row::render` is the rail single-row path with a fixed column. A key
        // past the column truncates and still keeps one space before the value,
        // so it can never jam. (The flat grid instead grows the column via
        // tabled and does not truncate.)
        let row = Row::new(Glyph::Done, "providers discovered", "9 providers");
        let plain = strip_ansi(&row.render());
        assert!(plain.contains("providers dis… 9 providers"), "{plain:?}");
    }
}
