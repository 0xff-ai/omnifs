//! Mount color palette. First-sight assignment from a curated list;
//! cycles deterministically when exceeded so screenshots remain stable
//! across reorderings.

use std::collections::HashMap;

use ratatui::style::Color;

/// Eight visually distinct colors for mount accents. Chosen for legible
/// contrast on both light and dark terminal themes.
const PALETTE: &[Color] = &[
    Color::Cyan,
    Color::Yellow,
    Color::LightGreen,
    Color::LightMagenta,
    Color::LightBlue,
    Color::LightRed,
    Color::LightCyan,
    Color::LightYellow,
];

#[derive(Debug, Default, Clone)]
pub struct MountPalette {
    assignments: HashMap<String, Color>,
    next_index: usize,
}

impl MountPalette {
    /// Return the stable color for this mount, allocating on first sight.
    pub fn color_for(&mut self, mount: &str) -> Color {
        if let Some(color) = self.assignments.get(mount) {
            return *color;
        }
        let color = PALETTE[self.next_index % PALETTE.len()];
        self.next_index += 1;
        self.assignments.insert(mount.to_string(), color);
        color
    }

    /// Look up without allocating (useful for render-only paths).
    pub fn peek(&self, mount: &str) -> Option<Color> {
        self.assignments.get(mount).copied()
    }
}
