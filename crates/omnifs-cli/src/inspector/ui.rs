//! Ratatui rendering: header, sparkline strip, tree | operations log.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use omnifs_inspector::InspectorOutcome;

use super::app::{App, ConnectionMode, PaneFocus};
use super::filter::FilterMode;
use super::format;
use super::metrics::{MountWindow, render_sparkline};
use super::trace_state::{Operation, OperationStatus, Stage, StageKind};
use super::tree::{ACTIVE_FOCUS_WINDOW_US, NodeStatus, RenderRow};

const CURSOR_BG: Color = Color::Rgb(40, 50, 60);

const SPARK_BUCKETS: usize = 12;
const SPARK_MOUNT_CAP: usize = 8;

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if format::compact_mode(area.width, area.height) {
        render_compact(frame, app, area);
        return;
    }

    let mount_count = app.ordered_mounts_for_strip(SPARK_MOUNT_CAP).len().max(1);
    let strip_height = u16::try_from(mount_count)
        .unwrap_or(u16::try_from(SPARK_MOUNT_CAP).unwrap_or(u16::MAX))
        + 2;

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(strip_height),
        Constraint::Min(8),
    ])
    .split(area);

    render_header(frame, app, chunks[0]);
    render_sparkline_strip(frame, app, chunks[1]);
    render_main(frame, app, chunks[2]);
}

fn render_compact(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(4)]).split(area);
    render_header(frame, app, chunks[0]);
    render_operations_log(frame, app, chunks[1]);
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let source = match app.mode {
        ConnectionMode::Inspector => {
            // When disconnected, surface the address we're failing to
            // reach so the user can tell "no peer listening" apart from
            // "peer connected but quiet".
            let state = if app.connected {
                "connected".to_string()
            } else {
                match &app.addr {
                    Some(addr) => format!("waiting on {addr}"),
                    None => "disconnected".to_string(),
                }
            };
            format!("live · {} · {state}", app.container)
        },
        ConnectionMode::Replay => format!("replay · {}", app.container),
    };
    let pause = if app.paused { " paused" } else { "" };
    let filter = match app.filter.mode {
        FilterMode::All => "",
        FilterMode::ErrorsOnly => " errors-only",
    };
    let edit = if app.filter.editing {
        format!(" filter:{}", app.filter.query)
    } else if !app.filter.query.is_empty() {
        format!(" filter={}", app.filter.query)
    } else {
        String::new()
    };
    let title = format!(
        " omnifs inspect │ {source}{pause} │ {:.1} evt/s │ dropped {} ",
        app.events_per_sec, app.dropped_events
    );
    let keys = " q quit  tab focus  ↑/↓ navigate  ↵ collapse  space pause  e errors  i idle  / filter  r reset ";
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title + filter + &edit)
        .title_bottom(keys);
    frame.render_widget(block, area);
}

fn render_sparkline_strip(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" mounts ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mounts = app.ordered_mounts_for_strip(SPARK_MOUNT_CAP);
    if mounts.is_empty() {
        let msg =
            Paragraph::new("waiting for activity…").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(msg, inner);
        return;
    }

    let mut lines = Vec::with_capacity(mounts.len());
    let empty_window = MountWindow::default();
    for mount in &mounts {
        let window = app.mount_window(mount).unwrap_or(&empty_window);
        let color = app.palette().peek(mount).unwrap_or(Color::DarkGray);
        lines.push(sparkline_line(mount, window, color, app.now_mono));
    }
    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

fn sparkline_line(mount: &str, window: &MountWindow, color: Color, now_mono: u64) -> Line<'static> {
    let buckets = window.sparkline(now_mono, SPARK_BUCKETS);
    let bars = render_sparkline(&buckets);
    let rate = window.event_rate_per_sec(now_mono);
    let err = window.error_rate();
    let cache = window
        .cache_hit_ratio()
        .map_or_else(|| "  —".to_string(), |r| format!("{:>3.0}%", r * 100.0));
    let p95 = window
        .p95_latency_us()
        .map_or_else(|| "—".to_string(), format::format_latency_us);
    let idle_label = window.is_empty();
    let mount_styled = Span::styled(
        format!("  {mount:<10}"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    );
    let spans = if idle_label {
        vec![
            mount_styled,
            Span::styled(
                format!("  {:<SPARK_BUCKETS$}", "─".repeat(SPARK_BUCKETS)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled("   idle", Style::default().fg(Color::DarkGray)),
        ]
    } else {
        vec![
            mount_styled,
            Span::styled(format!("  {bars}"), Style::default().fg(color)),
            Span::raw(format!("   evt/s {rate:>4.1}")),
            Span::raw(format!("   err {:>3.0}%", err * 100.0)),
            Span::raw(format!("   cache {cache}")),
            Span::raw(format!("   p95 {p95}")),
        ]
    };
    Line::from(spans)
}

fn render_main(frame: &mut Frame, app: &App, area: Rect) {
    let columns =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).split(area);
    render_tree(frame, app, columns[0]);
    render_operations_log(frame, app, columns[1]);
}

fn render_tree(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" tree ")
        .border_style(pane_border_style(app, PaneFocus::Tree));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = app
        .forest()
        .render_rows(app.now_mono, ACTIVE_FOCUS_WINDOW_US);
    if rows.is_empty() {
        let msg =
            Paragraph::new("no paths touched yet").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(msg, inner);
        return;
    }

    let lines: Vec<Line<'static>> = rows
        .iter()
        .map(|row| TreeRowView { app, row }.into_line())
        .collect();
    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

fn pane_border_style(app: &App, pane: PaneFocus) -> Style {
    if app.focus == pane {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// One tree-pane row, ready to be rendered as a `Line`. Holds the row
/// data plus the rendering context (palette, cursor) that turn raw
/// `RenderRow` fields into styled spans.
struct TreeRowView<'a> {
    app: &'a App,
    row: &'a RenderRow,
}

impl TreeRowView<'_> {
    fn into_line(self) -> Line<'static> {
        let Self { app, row } = self;
        let mount_color = app.palette().peek(&row.mount).unwrap_or(Color::White);
        let glyph_color = match row.status {
            NodeStatus::Error => Color::LightRed,
            NodeStatus::InFlight => Color::LightYellow,
            NodeStatus::RecentHit => Color::LightGreen,
            NodeStatus::Cached => mount_color,
            NodeStatus::Untouched => Color::DarkGray,
        };
        let name_style = if row.depth == 0 {
            Style::default()
                .fg(mount_color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let is_cursor = app
            .tree_cursor
            .as_ref()
            .is_some_and(|c| c.mount == row.mount && c.path == row.path);

        let mut spans = vec![
            Span::raw("  ".repeat(row.depth)),
            Span::styled(
                format!("{} ", row.status.glyph()),
                Style::default().fg(glyph_color),
            ),
            Span::styled(row.name.clone(), name_style),
        ];
        if row.is_subtree_handoff {
            spans.push(Span::styled(
                "  ▸",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if row.in_flight > 0 && row.status != NodeStatus::InFlight {
            spans.push(Span::styled(
                format!("  ◆{}", row.in_flight),
                Style::default().fg(Color::LightYellow),
            ));
        }
        if row.errors_below > 0 && row.status != NodeStatus::Error {
            spans.push(Span::styled(
                format!("  ✗{}", row.errors_below),
                Style::default().fg(Color::LightRed),
            ));
        }
        if let Some(us) = row.last_latency_us {
            spans.push(Span::styled(
                format!("  {}", format::format_latency_us(us)),
                Style::default().fg(Color::DarkGray),
            ));
        }
        let mut line = Line::from(spans);
        if is_cursor {
            // patch_style layers a bg under each span without inverting
            // their fg. REVERSED would swap each span's fg into its bg,
            // producing a mosaic of colored backgrounds instead of a
            // uniform highlight band.
            line = line.patch_style(Style::default().bg(CURSOR_BG));
        }
        line
    }
}

fn render_operations_log(frame: &mut Frame, app: &App, area: Rect) {
    let title = format!(
        " operations ({} retained / {} cap) ",
        app.retained_trace_count(),
        App::max_retained_traces()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(pane_border_style(app, PaneFocus::OpsLog));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let capacity = inner.height as usize;
    let width = inner.width as usize;
    if capacity == 0 || width == 0 {
        return;
    }

    let trace_ids = app.visible_trace_ids();
    let blocks: Vec<(omnifs_inspector::TraceId, Vec<Line<'static>>)> = trace_ids
        .iter()
        .filter_map(|&tid| {
            let op = app.operation(tid)?;
            Some((tid, OperationBlockView { op, app, width }.into_lines()))
        })
        .collect();

    // Newest-first rendering; advance `start` until the selected trace's
    // full block fits within `capacity`. Each block carries a trailing
    // separator row so blocks visually break apart.
    let selected = app.selected_trace();
    let start = scroll_start_for_selected(&blocks, selected, capacity);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(capacity);
    for (tid, block) in blocks.iter().skip(start) {
        let is_selected = selected == Some(*tid);
        if lines.len() + block.len() > capacity {
            break;
        }
        for raw in block {
            let mut line = raw.clone();
            if is_selected {
                line = line.patch_style(Style::default().bg(CURSOR_BG));
            }
            lines.push(line);
        }
        if lines.len() < capacity {
            lines.push(Line::raw(""));
        }
        if lines.len() >= capacity {
            break;
        }
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

/// Pick the starting block index so the selected trace's block fits in
/// `capacity` rows. Returns 0 when nothing is selected or already in view.
fn scroll_start_for_selected(
    blocks: &[(omnifs_inspector::TraceId, Vec<Line<'static>>)],
    selected: Option<omnifs_inspector::TraceId>,
    capacity: usize,
) -> usize {
    let Some(sel) = selected else { return 0 };
    let Some(sel_idx) = blocks.iter().position(|(tid, _)| *tid == sel) else {
        return 0;
    };
    let mut start = 0;
    loop {
        let total: usize = blocks[start..=sel_idx]
            .iter()
            .map(|(_, b)| b.len() + 1)
            .sum();
        if total <= capacity || start >= sel_idx {
            return start;
        }
        start += 1;
    }
}

/// All the lines that make up one trace block in the operations log:
/// a header row identifying the FUSE request, indented stage rows
/// (provider work, callouts, cache events, subtree handoffs, clones),
/// and a result row showing total elapsed + outcome.
struct OperationBlockView<'a> {
    op: &'a Operation,
    app: &'a App,
    width: usize,
}

impl OperationBlockView<'_> {
    fn into_lines(self) -> Vec<Line<'static>> {
        let Self { op, app, width } = self;
        let mount_color = app.palette().peek(&op.mount).unwrap_or(Color::White);
        let path = format::shorten_path(&op.path, width.saturating_sub(16).max(8));
        let marker = if op.status == OperationStatus::Running {
            "▦"
        } else {
            "●"
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format!("{marker} #{} ", op.trace_id),
                Style::default().fg(mount_color),
            ),
            Span::styled(
                format!("{} ", op.mount),
                Style::default()
                    .fg(mount_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{} ", op.fuse_op)),
            Span::styled(path, Style::default().fg(Color::White)),
        ])];

        for stage in op.stages.iter().skip(1) {
            if let Some(line) = (StageView { stage, width }).into_line() {
                lines.push(line);
            }
        }

        if let Some(elapsed) = op.fuse_elapsed_us {
            let outcome = op.outcome.unwrap_or(InspectorOutcome::Ok);
            let outcome_color = if outcome == InspectorOutcome::Ok {
                Color::LightGreen
            } else {
                Color::LightRed
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("◀ ", Style::default().fg(outcome_color)),
                Span::styled(
                    outcome.to_string(),
                    Style::default()
                        .fg(outcome_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {}", format::format_latency_us(elapsed))),
            ]));
        } else if op.status == OperationStatus::Running {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("◀ …", Style::default().fg(Color::DarkGray)),
            ]));
        }
        lines
    }
}

/// One stage rendered as a three-column row: `{indent}{glyph} {display}`
/// left-aligned, padded, then elapsed + outcome trailing at the right
/// edge. Returns `None` for marker stages (`provider.suspend` /
/// `provider.resume`) which the block view folds into the surrounding
/// provider stage.
struct StageView<'a> {
    stage: &'a Stage,
    width: usize,
}

/// Visual cell of a `StageView`: indent, glyph, glyph color, and the
/// formatted display string for the stage's left half. Selected by
/// pattern-matching on `Stage::kind` so the mapping is exhaustive
/// and the renderer doesn't string-parse the label per frame.
struct StageCell {
    indent: &'static str,
    glyph: &'static str,
    glyph_color: Color,
    display: String,
}

impl StageCell {
    /// Pick the visual cell for a stage based on its kind. Returns
    /// `None` for marker stages (`provider.suspend` / `provider.resume`)
    /// which the block view folds into the surrounding provider stage.
    fn for_stage(stage: &Stage) -> Option<Self> {
        match &stage.kind {
            StageKind::ProviderSuspend | StageKind::ProviderResume => None,
            StageKind::Provider(method) => Some(Self {
                indent: "  ",
                glyph: "▸",
                glyph_color: Color::LightCyan,
                display: method.clone(),
            }),
            StageKind::Callout(_) => Some(Self {
                indent: "    ",
                glyph: "◇",
                glyph_color: Color::LightYellow,
                display: stage.detail.clone(),
            }),
            // Keep the `cache.<kind>` prefix in the visible text so
            // a row like `◐ cache.browse_hit /github/...` reads
            // unambiguously without relying on the user knowing what
            // the ◐ glyph means.
            StageKind::Cache(_) => Some(Self {
                indent: "  ",
                glyph: "◐",
                glyph_color: Color::LightGreen,
                display: format!("{} {}", stage.kind.display_label(), stage.detail),
            }),
            StageKind::SubtreeStart | StageKind::SubtreeEnd => Some(Self {
                indent: "  ",
                glyph: "▸",
                glyph_color: Color::Magenta,
                display: format!("{} {}", stage.kind.display_label(), stage.detail),
            }),
            StageKind::CloneStart | StageKind::CloneEnd => Some(Self {
                indent: "    ",
                glyph: "⇣",
                glyph_color: Color::LightMagenta,
                display: format!("{} {}", stage.kind.display_label(), stage.detail),
            }),
            StageKind::Fuse(_) => Some(Self {
                indent: "  ",
                glyph: "·",
                glyph_color: Color::DarkGray,
                display: stage.kind.display_label().into_owned(),
            }),
        }
    }
}

impl StageView<'_> {
    fn into_line(self) -> Option<Line<'static>> {
        let StageView { stage, width } = self;
        let cell = StageCell::for_stage(stage)?;

        let (elapsed_text, outcome_text, outcome_color) =
            match (stage.elapsed_us, stage.outcome, stage.in_flight) {
                (Some(us), Some(o), _) => {
                    let color = if o == InspectorOutcome::Ok {
                        Color::DarkGray
                    } else {
                        Color::LightRed
                    };
                    (format::format_latency_us(us), Some(o.to_string()), color)
                },
                (Some(us), None, _) => (format::format_latency_us(us), None, Color::DarkGray),
                (None, _, true) => (String::new(), Some("…".into()), Color::DarkGray),
                (None, _, false) => (String::new(), None, Color::DarkGray),
            };

        // Build the trailing chunk first so we know its width, then pad
        // between the left half and it. Stage content is overwhelmingly
        // ASCII so char count is a fair stand-in for cell width.
        let trailing = match (elapsed_text.as_str(), outcome_text.as_deref()) {
            ("", None) => String::new(),
            (elapsed, None) => elapsed.to_string(),
            ("", Some(out)) => out.to_string(),
            (elapsed, Some(out)) => format!("{elapsed} {out}"),
        };
        let leading_cols = cell.indent.chars().count()
            + cell.glyph.chars().count()
            + 1
            + cell.display.chars().count();
        let pad = width
            .saturating_sub(leading_cols)
            .saturating_sub(trailing.chars().count())
            .max(2);

        let mut spans = vec![
            Span::raw(cell.indent),
            Span::styled(
                format!("{} ", cell.glyph),
                Style::default().fg(cell.glyph_color),
            ),
            Span::raw(cell.display),
            Span::raw(" ".repeat(pad)),
        ];
        if !elapsed_text.is_empty() {
            spans.push(Span::styled(
                elapsed_text,
                Style::default().fg(Color::DarkGray),
            ));
        }
        if let Some(outcome) = outcome_text {
            let sep = if spans
                .last()
                .is_some_and(|s| !s.content.chars().all(char::is_whitespace))
            {
                " "
            } else {
                ""
            };
            spans.push(Span::styled(
                format!("{sep}{outcome}"),
                Style::default().fg(outcome_color),
            ));
        }
        Some(Line::from(spans))
    }
}
