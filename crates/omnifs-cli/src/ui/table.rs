//! Human-only responsive resource tables.
//!
//! This module deliberately has no serde surface and no knowledge of mounts,
//! providers, or frontends. Commands provide cells and state tokens; the
//! renderer owns layout, terminal width, and action placement.

use std::fmt::Write as _;
use std::io::IsTerminal as _;
use unicode_width::UnicodeWidthChar as _;

use crossterm::terminal;

/// A complete human report made up of context strips and resource tables.
#[derive(Debug, Clone, Default)]
pub(crate) struct Report {
    pub(crate) blocks: Vec<Block>,
}

impl Report {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, block: Block) {
        self.blocks.push(block);
    }

    /// Render using the current terminal when possible. Piped output uses a
    /// stable width and never contains ANSI escape sequences.
    pub(crate) fn render(&self) -> String {
        let is_tty = std::io::stdout().is_terminal();
        let width = if is_tty {
            terminal::size().map_or(80, |(width, _)| width as usize)
        } else {
            120
        };
        self.render_with(RenderOptions {
            width,
            // The stdout color decision has one owner (`style::color_enabled`)
            // so `NO_COLOR` and `CLICOLOR_FORCE` behave identically here and
            // on every other stdout-bound render.
            color: super::style::color_enabled(super::style::Stream::Stdout),
        })
    }

    pub(crate) fn print(&self) {
        anstream::print!("{}", self.render());
    }

    /// Render with explicitly injected terminal capabilities. Tests and
    /// callers that capture output should use this method instead of probing
    /// the process terminal.
    pub(crate) fn render_with(&self, options: RenderOptions) -> String {
        let mut out = String::new();
        for (index, block) in self.blocks.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            match block {
                Block::Context(context) => context.render_into(&mut out, options),
                Block::Resources(table) => table.render_into(&mut out, options),
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Block {
    Context(ContextStrip),
    Resources(ResourceTable),
}

#[derive(Debug, Clone)]
pub(crate) struct ContextStrip {
    pub(crate) title: String,
    pub(crate) location: String,
    pub(crate) state: StateToken,
    pub(crate) metadata: Vec<Meta>,
    pub(crate) action: Option<Action>,
}

impl ContextStrip {
    pub(crate) fn new(
        title: impl Into<String>,
        location: impl Into<String>,
        state: StateToken,
    ) -> Self {
        Self {
            title: title.into(),
            location: location.into(),
            state,
            metadata: Vec::new(),
            action: None,
        }
    }

    pub(crate) fn with_metadata(mut self, metadata: impl IntoIterator<Item = Meta>) -> Self {
        self.metadata.extend(metadata);
        self
    }

    pub(crate) fn with_action(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }

    fn render_into(&self, out: &mut String, options: RenderOptions) {
        let left = format!("{}  {}", self.title, self.location);
        let state = self.state.render(options.color);
        let gap = options
            .width
            .saturating_sub(display_width(&left) + display_width(&state))
            .max(2);
        let _ = writeln!(out, "{}{}{}", left, " ".repeat(gap), state);
        if !self.metadata.is_empty() {
            let _ = writeln!(
                out,
                "{}",
                self.metadata
                    .iter()
                    .map(Meta::render)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(action) = self.action.as_ref() {
            let _ = writeln!(out, "  {}  {}", action.label, action.command);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Meta {
    pub(crate) label: String,
    pub(crate) value: String,
}

impl Meta {
    pub(crate) fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }

    fn render(&self) -> String {
        format!("{} {}", self.label, self.value)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResourceTable {
    pub(crate) title: String,
    pub(crate) count: CountLabel,
    pub(crate) columns: Vec<Column>,
    pub(crate) rows: Vec<ResourceRow>,
}

impl ResourceTable {
    pub(crate) fn new(
        title: impl Into<String>,
        count: impl Into<CountLabel>,
        columns: Vec<Column>,
    ) -> Self {
        Self {
            title: title.into(),
            count: count.into(),
            columns,
            rows: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, row: ResourceRow) {
        self.rows.push(row);
    }

    fn heading(&self) -> String {
        format!("{}  {}", self.title, self.count.0)
    }

    fn render_into(&self, out: &mut String, options: RenderOptions) {
        let _ = writeln!(out, "{}", self.heading());
        if self.rows.is_empty() {
            let _ = writeln!(out, "  (none)");
            return;
        }

        if options.width <= 71 {
            self.render_stacked(out, options);
        } else {
            self.render_wide(out, options);
        }
    }

    fn render_wide(&self, out: &mut String, options: RenderOptions) {
        let active = self.active_columns(options.width);
        let widths = self.column_widths(&active);
        let shared = shared_actions(&self.rows);

        let mut header = String::new();
        for (position, column_index) in active.iter().enumerate() {
            let column = &self.columns[*column_index];
            header.push_str("  ");
            header.push_str(&pad_right(column.heading, widths[position]));
        }
        let _ = writeln!(out, "{}", header.trim_end());

        for row in &self.rows {
            let mut line = String::from("  ");
            for (position, column_index) in active.iter().enumerate() {
                let value = row
                    .cells
                    .get(*column_index)
                    .map_or_else(String::new, |cell| {
                        column_value(
                            &cell.render(options.color),
                            self.columns[*column_index].width,
                        )
                    });
                line.push_str("  ");
                line.push_str(&pad_right(&value, widths[position]));
            }
            let _ = writeln!(out, "{}", line.trim_end());
            if let Some(action) = row.action.as_ref()
                && !shared.contains(&action.command)
            {
                let _ = writeln!(out, "    {}  {}", action.label, action.command);
            }
        }
        for action in shared_actions_for_render(&self.rows, &shared) {
            let _ = writeln!(out, "  {}  {}", action.label, action.command);
        }
    }

    fn render_stacked(&self, out: &mut String, options: RenderOptions) {
        let shared = shared_actions(&self.rows);
        for row in &self.rows {
            let identity = self
                .columns
                .iter()
                .enumerate()
                .filter(|(_, column)| column.priority == Priority::Identity)
                .filter_map(|(index, column)| {
                    row.cells
                        .get(index)
                        .map(|cell| column_value(&cell.render(options.color), column.width))
                })
                .collect::<Vec<_>>();
            let left = format!("  {}", identity.join("  "));
            let state = row.state.render(options.color);
            let gap = options
                .width
                .saturating_sub(display_width(&left) + display_width(&state))
                .max(2);
            let _ = writeln!(out, "{}{}{}", left, " ".repeat(gap), state);
            let metadata = self
                .columns
                .iter()
                .enumerate()
                .filter(|(_, column)| column.priority != Priority::Identity)
                .filter_map(|(index, column)| {
                    row.cells.get(index).map(|cell| {
                        format!(
                            "{}  {}",
                            column.heading,
                            column_value(&cell.render(options.color), column.width)
                        )
                    })
                })
                .collect::<Vec<_>>();
            if !metadata.is_empty() {
                let _ = writeln!(out, "    {}", metadata.join(", "));
            }
            if let Some(action) = row.action.as_ref()
                && !shared.contains(&action.command)
            {
                let _ = writeln!(out, "    {}  {}", action.label, action.command);
            }
        }
        for action in shared_actions_for_render(&self.rows, &shared) {
            let _ = writeln!(out, "  {}  {}", action.label, action.command);
        }
    }

    fn active_columns(&self, width: usize) -> Vec<usize> {
        let mut active: Vec<usize> = (0..self.columns.len()).collect();
        if width < 100 {
            active.retain(|index| self.columns[*index].priority != Priority::Detail);
        }
        while self.layout_width(&active) > width {
            let removable = active
                .iter()
                .find(|index| self.columns[**index].priority == Priority::Secondary);
            let Some(index) = removable.copied() else {
                break;
            };
            active.retain(|candidate| *candidate != index);
        }
        active
    }

    fn column_widths(&self, active: &[usize]) -> Vec<usize> {
        active
            .iter()
            .map(|index| {
                let column = &self.columns[*index];
                let heading = display_width(column.heading);
                let content = self
                    .rows
                    .iter()
                    .filter_map(|row| row.cells.get(*index))
                    .map(|cell| display_width(&column_value(&cell.render(false), column.width)))
                    .max()
                    .unwrap_or(0);
                heading.max(content)
            })
            .collect()
    }

    fn layout_width(&self, active: &[usize]) -> usize {
        2 + active
            .iter()
            .map(|index| {
                let column = &self.columns[*index];
                let heading = display_width(column.heading);
                let content = self
                    .rows
                    .iter()
                    .filter_map(|row| row.cells.get(*index))
                    .map(|cell| display_width(&column_value(&cell.render(false), column.width)))
                    .max()
                    .unwrap_or(0);
                heading.max(content) + 2
            })
            .sum::<usize>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CountLabel(String);

impl CountLabel {
    pub(crate) fn number(count: usize) -> Self {
        Self(count.to_string())
    }

    pub(crate) fn named(count: usize, noun: &'static str) -> Self {
        Self(format!("{count} {noun}"))
    }
}

impl From<usize> for CountLabel {
    fn from(count: usize) -> Self {
        Self::number(count)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Column {
    pub(crate) heading: &'static str,
    pub(crate) priority: Priority,
    pub(crate) width: WidthPolicy,
}

impl Column {
    pub(crate) const fn new(heading: &'static str, priority: Priority, width: WidthPolicy) -> Self {
        Self {
            heading,
            priority,
            width,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Priority {
    Identity,
    Essential,
    Secondary,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WidthPolicy {
    Auto,
    Path,
}

#[derive(Debug, Clone)]
pub(crate) struct ResourceRow {
    pub(crate) cells: Vec<Cell>,
    pub(crate) state: StateToken,
    pub(crate) action: Option<Action>,
}

impl ResourceRow {
    pub(crate) fn new(cells: impl IntoIterator<Item = Cell>, state: StateToken) -> Self {
        Self {
            cells: cells.into_iter().collect(),
            state,
            action: None,
        }
    }

    pub(crate) fn with_action(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Cell {
    Text(String),
    State(StateToken),
}

impl Cell {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    pub(crate) fn state(value: StateToken) -> Self {
        Self::State(value)
    }

    fn render(&self, color: bool) -> String {
        match self {
            Self::Text(value) => value.clone(),
            Self::State(state) => state.render(color),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Action {
    label: &'static str,
    pub(crate) command: String,
}

impl Action {
    pub(crate) fn fix(command: impl Into<String>) -> Self {
        Self {
            label: "Fix",
            command: command.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Positive,
    Neutral,
    Attention,
    Failure,
}

impl Severity {
    pub(crate) const fn symbol(self) -> char {
        match self {
            Self::Positive => '●',
            Self::Neutral => '○',
            Self::Attention => '▲',
            Self::Failure => '×',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateToken {
    severity: Severity,
    label: String,
}

impl StateToken {
    pub(crate) fn positive(label: impl Into<String>) -> Self {
        Self::new(Severity::Positive, label)
    }

    pub(crate) fn neutral(label: impl Into<String>) -> Self {
        Self::new(Severity::Neutral, label)
    }

    pub(crate) fn attention(label: impl Into<String>) -> Self {
        Self::new(Severity::Attention, label)
    }

    pub(crate) fn failure(label: impl Into<String>) -> Self {
        Self::new(Severity::Failure, label)
    }

    pub(crate) fn new(severity: Severity, label: impl Into<String>) -> Self {
        Self {
            severity,
            label: label.into().to_lowercase(),
        }
    }

    pub(crate) fn symbol(&self) -> char {
        self.severity.symbol()
    }

    fn render(&self, color: bool) -> String {
        let token = format!("{} {}", self.symbol(), self.label);
        if !color {
            return token;
        }
        let ansi = match self.severity {
            Severity::Positive => "\u{1b}[32m",
            Severity::Neutral => "\u{1b}[2m",
            Severity::Attention => "\u{1b}[33m",
            Severity::Failure => "\u{1b}[31m",
        };
        format!("{ansi}{token}\u{1b}[0m")
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RenderOptions {
    pub(crate) width: usize,
    pub(crate) color: bool,
}

fn column_value(value: &str, policy: WidthPolicy) -> String {
    match policy {
        WidthPolicy::Auto => value.to_owned(),
        WidthPolicy::Path => truncate_display(value, 32),
    }
}

fn shared_actions(rows: &[ResourceRow]) -> Vec<String> {
    let mut commands = Vec::new();
    for command in rows
        .iter()
        .filter_map(|row| row.action.as_ref().map(|action| action.command.clone()))
    {
        if !commands.contains(&command) {
            commands.push(command);
        }
    }
    commands
        .into_iter()
        .filter(|command| {
            rows.iter()
                .filter(|row| {
                    row.action
                        .as_ref()
                        .is_some_and(|action| action.command == *command)
                })
                .count()
                > 1
        })
        .collect()
}

fn shared_actions_for_render<'a>(rows: &'a [ResourceRow], commands: &[String]) -> Vec<&'a Action> {
    commands
        .iter()
        .filter_map(|command| {
            rows.iter().find_map(|row| {
                row.action
                    .as_ref()
                    .filter(|action| action.command == *command)
            })
        })
        .collect()
}

fn display_width(text: &str) -> usize {
    super::strip_ansi(text)
        .chars()
        .map(|ch| ch.width().unwrap_or(0))
        .sum()
}

fn truncate_display(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_owned();
    }
    if max_width <= 1 {
        return "…".to_owned();
    }
    let mut output = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let width = ch.width().unwrap_or(0);
        if used + width + 1 > max_width {
            break;
        }
        output.push(ch);
        used += width;
    }
    output.push('…');
    output
}

fn pad_right(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(value));
    let mut padded = String::with_capacity(value.len() + padding);
    padded.push_str(value);
    padded.push_str(&" ".repeat(padding));
    padded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ResourceTable {
        let columns = vec![
            Column::new("Name", Priority::Identity, WidthPolicy::Auto),
            Column::new("Location", Priority::Essential, WidthPolicy::Path),
            Column::new("Digest", Priority::Secondary, WidthPolicy::Auto),
            Column::new("Details", Priority::Detail, WidthPolicy::Auto),
        ];
        let action = Action::fix("omnifs mount reauth github");
        let mut table = ResourceTable::new("Frontends", 2, columns);
        table.push(
            ResourceRow::new(
                [
                    Cell::new("github"),
                    Cell::new("/very/long/location"),
                    Cell::new("abcdef0123456789"),
                    Cell::new("oauth"),
                ],
                StateToken::attention("reauth required"),
            )
            .with_action(action.clone()),
        );
        table.push(
            ResourceRow::new(
                [
                    Cell::new("linear"),
                    Cell::new("/other/location"),
                    Cell::new("abcdef0123456789"),
                    Cell::new("oauth"),
                ],
                StateToken::attention("reauth required"),
            )
            .with_action(action),
        );
        table
    }

    #[test]
    fn wide_layout_has_sentence_case_heading_and_no_rules() {
        let mut report = Report::new();
        report.push(Block::Resources(sample()));
        let output = report.render_with(RenderOptions {
            width: 120,
            color: false,
        });
        assert!(output.starts_with("Frontends  2\n"));
        assert!(output.contains("Name"));
        assert!(output.contains("Location"));
        assert!(!output.contains('│'));
        assert!(!output.contains("---"));
        assert!(!output.contains("DETAILS"));
    }

    #[test]
    fn intermediate_width_removes_detail_before_secondary() {
        let mut report = Report::new();
        report.push(Block::Resources(sample()));
        let output = report.render_with(RenderOptions {
            width: 80,
            color: false,
        });
        assert!(!output.contains("Details"));
        assert!(output.contains("Digest"));
        assert!(output.contains("Location"));
    }

    #[test]
    fn very_tight_wide_layout_then_removes_secondary() {
        let mut table = sample();
        table.columns[1].width = WidthPolicy::Auto;
        table.rows[0].cells[1] = Cell::new("x".repeat(60));
        let active = table.active_columns(80);
        assert!(active.contains(&1));
        assert!(!active.contains(&2));
        assert!(!active.contains(&3));
    }

    #[test]
    fn declared_state_columns_keep_their_schema_order() {
        let mut table = ResourceTable::new(
            "Mounts",
            1,
            vec![
                Column::new("Name", Priority::Identity, WidthPolicy::Auto),
                Column::new("Auth", Priority::Essential, WidthPolicy::Auto),
                Column::new("Serving", Priority::Essential, WidthPolicy::Auto),
            ],
        );
        table.push(ResourceRow::new(
            [
                Cell::new("github"),
                Cell::state(StateToken::positive("ready")),
                Cell::state(StateToken::neutral("stopped")),
            ],
            StateToken::positive("attached"),
        ));
        let mut report = Report::new();
        report.push(Block::Resources(table));
        let plain = crate::ui::strip_ansi(&report.render_with(RenderOptions {
            width: 120,
            color: true,
        }));
        assert!(plain.find("Name").unwrap() < plain.find("Auth").unwrap());
        assert!(plain.find("Auth").unwrap() < plain.find("Serving").unwrap());
        assert!(plain.contains("● ready"));
        assert!(plain.contains("○ stopped"));
    }

    #[test]
    fn context_strip_has_two_line_core_and_optional_action() {
        let context = ContextStrip::new("Workspace", "~/.omnifs", StateToken::positive("ready"))
            .with_metadata([Meta::new("daemon", "running"), Meta::new("mounts", "2")])
            .with_action(Action::fix("omnifs up"));
        let mut report = Report::new();
        report.push(Block::Context(context));
        let output = report.render_with(RenderOptions {
            width: 80,
            color: false,
        });
        assert_eq!(output.lines().count(), 3);
        assert!(
            output
                .lines()
                .nth(1)
                .unwrap()
                .contains("daemon running, mounts 2")
        );
        assert!(output.lines().nth(2).unwrap().contains("Fix  omnifs up"));
    }

    #[test]
    fn narrow_layout_stacks_identity_metadata_and_one_shared_action() {
        let mut report = Report::new();
        report.push(Block::Resources(sample()));
        let output = report.render_with(RenderOptions {
            width: 71,
            color: false,
        });
        let first_row = output
            .lines()
            .find(|line| line.contains("github"))
            .expect("first stacked resource row");
        let first_row = crate::ui::strip_ansi(first_row);
        assert!(first_row.contains("github"));
        assert!(first_row.contains("▲ reauth required"));
        assert!(first_row.find("github").unwrap() < first_row.find("▲ reauth required").unwrap());
        assert!(display_width(&first_row) <= 71);
        assert_eq!(output.matches("Fix  omnifs mount reauth github").count(), 1);
        assert!(output.contains("Location  /very/long/location"));
    }

    #[test]
    fn ansi_and_wide_unicode_are_counted_by_display_width() {
        assert_eq!(display_width("\u{1b}[31m火\u{1b}[0m"), 2);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(display_width("🚀"), 2);
        assert_eq!(truncate_display("東京駅", 5), "東京…");
        assert_eq!(display_width(&pad_right("火", 4)), 4);
    }

    #[test]
    fn rendered_rows_align_colored_and_wide_cells() {
        let mut table = ResourceTable::new(
            "Mounts",
            2,
            vec![
                Column::new("Name", Priority::Identity, WidthPolicy::Auto),
                Column::new("Location", Priority::Essential, WidthPolicy::Auto),
            ],
        );
        table.push(ResourceRow::new(
            [Cell::new("\u{1b}[31mé\u{1b}[0m"), Cell::new("one")],
            StateToken::positive("attached"),
        ));
        table.push(ResourceRow::new(
            [Cell::new("火"), Cell::new("two")],
            StateToken::neutral("stopped"),
        ));
        let mut report = Report::new();
        report.push(Block::Resources(table));
        let output = report.render_with(RenderOptions {
            width: 120,
            color: true,
        });
        let rows = output
            .lines()
            .map(crate::ui::strip_ansi)
            .filter(|line| line.contains("one") || line.contains("two"))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].find("one"), rows[1].find("two"));
    }

    #[test]
    fn empty_table_is_explicit() {
        let table = ResourceTable::new("Mounts", 0, vec![]);
        let mut report = Report::new();
        report.push(Block::Resources(table));
        assert!(
            report
                .render_with(RenderOptions {
                    width: 120,
                    color: false
                })
                .contains("(none)")
        );
    }
}
