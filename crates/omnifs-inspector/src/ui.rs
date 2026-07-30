//! Ratatui rendering: header, sparkline strip, tree | operations log.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use omnifs_api::events::InspectorOutcome;
use unicode_width::UnicodeWidthStr as _;

use super::app::{App, AppView, OperationOrder, PaneFocus};
use super::filter::FilterMode;
use super::format;
use super::keymap::{footer_text, help_lines};
use super::metrics::{LatencyWindow, SPARK_BUCKETS, render_sparkline};
use super::trace_state::{Operation, OperationStatus, Stage, StageKind};
use super::tree::{NodeStatus, RenderRow};

// Shared with `sandbox_ui`: reverse video works without color support, while
// `CURSOR_MARKER` keeps the selection visible in plain captures.
pub(super) const CURSOR_STYLE: Style = Style::new().add_modifier(Modifier::REVERSED);
const CURSOR_MARKER: &str = "› ";

const SPARK_MOUNT_CAP: usize = 8;

struct StageCell {
    indent: &'static str,
    glyph: &'static str,
    glyph_color: Color,
    display: String,
}

impl StageCell {
    fn for_stage(stage: &Stage) -> Self {
        match &stage.kind {
            StageKind::Provider(method) => Self {
                indent: "  ",
                glyph: "▸",
                glyph_color: Color::LightCyan,
                display: method.clone(),
            },
            StageKind::Callout(_) => Self {
                indent: "    ",
                glyph: "◇",
                glyph_color: Color::LightYellow,
                display: stage.detail.clone(),
            },
            StageKind::Cache(_) => Self {
                indent: "  ",
                glyph: "◐",
                glyph_color: Color::LightGreen,
                display: format!("{} {}", stage.kind.display_label(), stage.detail),
            },
            StageKind::SubtreeStart | StageKind::SubtreeEnd => Self {
                indent: "  ",
                glyph: "▸",
                glyph_color: Color::Magenta,
                display: format!("{} {}", stage.kind.display_label(), stage.detail),
            },
            StageKind::CloneStart | StageKind::CloneEnd => Self {
                indent: "    ",
                glyph: "⇣",
                glyph_color: Color::LightMagenta,
                display: format!("{} {}", stage.kind.display_label(), stage.detail),
            },
            StageKind::Fuse(_) => Self {
                indent: "  ",
                glyph: "·",
                glyph_color: Color::DarkGray,
                display: stage.kind.display_label().into_owned(),
            },
        }
    }
}

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if app.view == AppView::Sandbox {
        super::sandbox_ui::render(frame, app, area);
        render_help(frame, app, area);
        return;
    }
    if format::compact_mode(area.width, area.height) {
        render_compact(frame, app, area);
        render_help(frame, app, area);
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
    render_help(frame, app, area);
}

fn render_compact(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(4)]).split(area);
    render_header(frame, app, chunks[0]);
    render_operations_log(frame, app, chunks[1]);
}

/// Shared by both full-screen views: `sandbox_ui::render` calls this
/// too so the two views agree on connection state, pause state, and
/// filter status, differing only in the view name and key hints.
pub(super) fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let source = if app.is_replay {
        format!("replay {}", app.container)
    } else {
        // When disconnected, surface the address we're failing to reach so
        // the user can tell "no peer listening" apart from "peer connected
        // but quiet".
        let state = if app.connected {
            "connected".to_string()
        } else {
            match &app.addr {
                Some(addr) => format!("waiting on {addr}"),
                None => "disconnected".to_string(),
            }
        };
        format!("live {} · {state}", app.container)
    };
    let pause = if app.paused() {
        format!(" · paused +{}", app.buffered_since_pause())
    } else {
        String::new()
    };
    let filter = match app.filter.mode {
        FilterMode::All => "",
        FilterMode::ErrorsOnly => " · errors-only",
    };
    let idle = if app.hide_idle { " · idle-hidden" } else { "" };
    let order = match app.operation_order {
        OperationOrder::Recent => "",
        OperationOrder::Latency => " · latency-order",
    };
    let speed = if app.is_replay {
        format!(" {}", app.replay_speed.label())
    } else {
        String::new()
    };
    let edit = if app.filter.editing {
        format!(" filter:{}", app.filter.query)
    } else if !app.filter.query.is_empty() {
        format!(" filter={}", app.filter.query)
    } else {
        String::new()
    };
    let view_name = match app.view {
        AppView::Activity => "activity",
        AppView::Sandbox => "sandbox map",
    };
    let title = format!(
        " inspect · {view_name} · {source}{speed}{pause}{filter}{idle}{order} · {:.1}/s · drop {} ",
        app.events_per_sec, app.dropped_events
    );
    let notice = app
        .notice
        .as_deref()
        .map_or(String::new(), |notice| format!(" │ {notice}"));
    let keys = footer_text(app);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title + &edit + &notice)
        .title_bottom(keys);
    frame.render_widget(block, area);
}

fn render_help(frame: &mut Frame, app: &App, area: Rect) {
    if !app.help_open {
        return;
    }
    let lines = help_lines(app);
    let width = area.width.saturating_sub(4).min(52);
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines.join("\n")).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" keys · ?/esc close "),
        ),
        popup,
    );
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
    let empty_window = LatencyWindow::default();
    for mount in &mounts {
        let window = app.mount_window(mount).unwrap_or(&empty_window);
        let color = app.palette().peek(mount).unwrap_or(Color::DarkGray);
        lines.push(sparkline_line(mount, window, color, app.view_now_mono()));
    }
    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

fn sparkline_line(
    mount: &str,
    window: &LatencyWindow,
    color: Color,
    now_mono: u64,
) -> Line<'static> {
    let buckets = window.sparkline(now_mono, SPARK_BUCKETS);
    let bars = render_sparkline(&buckets);
    let rate = window.event_rate_per_sec(now_mono);
    let err = window.error_rate();
    let cache = window
        .cache_hit_ratio()
        .map_or_else(|| "  —".to_string(), |r| format!("{:>3.0}%", r * 100.0));
    let p95_us = window.p95_latency_us();
    let p95 = p95_us.map_or_else(|| "—".to_string(), format::format_latency_us);
    let p95_color = p95_us.map_or(Color::DarkGray, format::latency_color);
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
            Span::styled(format!("   p95 {p95}"), Style::default().fg(p95_color)),
        ]
    };
    Line::from(spans)
}

fn render_main(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::vertical([Constraint::Min(5), Constraint::Length(8)]).split(area);
    let columns =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(rows[0]);
    render_tree(frame, app, columns[0]);
    render_operations_log(frame, app, columns[1]);
    render_operation_detail(frame, app, rows[1]);
}

fn empty_state_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        "Nothing yet.",
        Style::default().fg(Color::DarkGray),
    ))];
    if let Some(path) = &app.teaching_path {
        lines.push(Line::from(vec![
            Span::raw("  Generate activity: "),
            Span::styled(format!("ls {path}"), Style::default().fg(Color::Cyan)),
        ]));
    }
    lines
}

fn render_tree(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" recent paths ")
        .border_style(pane_border_style(app, PaneFocus::Tree));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = app.visible_tree_rows();
    if rows.is_empty() {
        let msg = Paragraph::new(empty_state_lines(app));
        frame.render_widget(msg, inner);
        return;
    }

    let lines: Vec<Line<'static>> = rows.iter().map(|row| tree_row_line(app, row)).collect();
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

/// One tree-pane row, ready to be rendered as a `Line`: turns raw
/// `RenderRow` fields into styled spans using the rendering context
/// (palette, cursor) `app` provides.
fn tree_row_line(app: &App, row: &RenderRow) -> Line<'static> {
    let mount_color = app.palette().peek(&row.mount).unwrap_or(Color::White);
    let glyph_color = match row.status {
        NodeStatus::Error => Color::LightRed,
        NodeStatus::InFlight => Color::LightYellow,
        NodeStatus::RecentHit => Color::LightGreen,
        NodeStatus::Cached => mount_color,
        NodeStatus::Miss | NodeStatus::Untouched => Color::DarkGray,
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
        Span::raw(if is_cursor { CURSOR_MARKER } else { "  " }),
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
            format!("  {} failed paths", row.errors_below),
            Style::default().fg(Color::LightRed),
        ));
    }
    if let Some(us) = row.last_latency_us {
        spans.push(Span::styled(
            format!("  {}", format::format_latency_us(us)),
            Style::default().fg(format::latency_color(us)),
        ));
    }
    let mut line = Line::from(spans);
    if is_cursor {
        line = line.patch_style(CURSOR_STYLE);
    }
    line
}

fn render_operations_log(frame: &mut Frame, app: &App, area: Rect) {
    let title = format!(
        " operations · {} ({} / {}) ",
        match app.operation_order {
            OperationOrder::Recent => "recent",
            OperationOrder::Latency => "latency",
        },
        app.retained_trace_count(),
        App::max_retained_traces()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(pane_border_style(app, PaneFocus::OpsLog));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width as usize;
    if inner.height == 0 || width == 0 {
        return;
    }

    let trace_ids = app.visible_trace_ids();
    let rows: Vec<(omnifs_api::events::TraceId, Line<'static>)> = trace_ids
        .iter()
        .filter_map(|&tid| {
            let op = app.operation(tid)?;
            Some((tid, operation_row_line(app, op, width)))
        })
        .collect();

    if rows.is_empty() {
        let msg = Paragraph::new(empty_state_lines(app));
        frame.render_widget(msg, inner);
        return;
    }

    let selected = app.selected_trace();
    let capacity = usize::from(inner.height);
    let selected_index =
        selected.and_then(|selected| rows.iter().position(|(trace_id, _)| *trace_id == selected));
    let start = selected_index.map_or(0, |index| index.saturating_sub(capacity.saturating_sub(1)));
    let lines = rows
        .iter()
        .skip(start)
        .take(capacity)
        .map(|(trace_id, raw)| {
            let is_selected = selected == Some(*trace_id);
            let mut spans = vec![Span::raw(if is_selected { CURSOR_MARKER } else { "  " })];
            spans.extend(raw.spans.clone());
            let mut line = Line::from(spans);
            if is_selected {
                line = line.patch_style(CURSOR_STYLE);
            }
            line
        })
        .collect::<Vec<_>>();
    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

/// One operations-log row, ready to be rendered as a `Line`. `width` bounds
/// how much of `op.path` fits after the timestamp, mount, op, and elapsed
/// cells reserve their own space.
fn operation_row_line(app: &App, op: &Operation, width: usize) -> Line<'static> {
    let mount_color = app.palette().peek(&op.mount).unwrap_or(Color::White);
    let elapsed = op
        .fuse_elapsed_us
        .map_or_else(|| "running".into(), format::format_latency_us);
    let elapsed_color = op
        .fuse_elapsed_us
        .map_or(Color::LightYellow, format::latency_color);
    let timestamp = format::format_timestamp(&op.started_ts);
    let reserved = timestamp.len() + op.mount.len() + op.fuse_op.len() + elapsed.len() + 7;
    let path = format::shorten_path(&op.path, width.saturating_sub(reserved).max(8));
    let outcome_color = match op.status {
        OperationStatus::Running => Color::LightYellow,
        OperationStatus::Ok => Color::LightGreen,
        OperationStatus::Error => Color::LightRed,
    };
    Line::from(vec![
        Span::styled(
            format!("{timestamp} "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{} ", op.mount),
            Style::default()
                .fg(mount_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} ", op.fuse_op),
            Style::default().fg(outcome_color),
        ),
        Span::raw(path),
        Span::styled(format!("  {elapsed}"), Style::default().fg(elapsed_color)),
    ])
}

fn render_operation_detail(frame: &mut Frame, app: &App, area: Rect) {
    let selected = app
        .selected_trace()
        .and_then(|trace_id| app.operation(trace_id));
    let title = selected.map_or_else(
        || " selected operation ".to_string(),
        |operation| {
            format!(
                " {} · {} {} · trace {} ",
                operation.mount, operation.fuse_op, operation.path, operation.trace_id
            )
        },
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(operation) = selected else {
        return;
    };
    let width = usize::from(inner.width);
    let mut lines = operation
        .stages
        .iter()
        .map(|stage| stage_line(stage, width))
        .collect::<Vec<_>>();
    if let Some(elapsed) = operation.fuse_elapsed_us {
        let outcome = operation.outcome.unwrap_or(InspectorOutcome::Ok);
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {outcome}"),
                Style::default().fg(if outcome == InspectorOutcome::Ok {
                    Color::LightGreen
                } else {
                    Color::LightRed
                }),
            ),
            Span::styled(
                format!("  {}", format::format_latency_us(elapsed)),
                Style::default().fg(format::latency_color(elapsed)),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One stage rendered as a three-column row: `{indent}{glyph} {display}`
/// left-aligned, padded, then elapsed + outcome trailing at the right
/// edge.
fn stage_line(stage: &Stage, width: usize) -> Line<'static> {
    let cell = StageCell::for_stage(stage);

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

    let trailing = match (elapsed_text.as_str(), outcome_text.as_deref()) {
        ("", None) => String::new(),
        (elapsed, None) => elapsed.to_string(),
        ("", Some(out)) => out.to_string(),
        (elapsed, Some(out)) => format!("{elapsed} {out}"),
    };
    let leading_cols = cell.indent.width() + cell.glyph.width() + 1 + cell.display.width();
    let pad = width
        .saturating_sub(leading_cols)
        .saturating_sub(trailing.width())
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
        let elapsed_color = stage
            .elapsed_us
            .map_or(Color::DarkGray, format::latency_color);
        spans.push(Span::styled(
            elapsed_text,
            Style::default().fg(elapsed_color),
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
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use omnifs_api::events::InspectorLine;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::app::App;
    use crate::source::SourceMessage;

    fn fixture_app() -> App {
        let mut app = App::new(true, "fixture", None, Some("/omnifs/github".into()));
        for line in include_str!("../tests/fixtures/inspector_journey.jsonl").lines() {
            app.apply_line(InspectorLine::parse_line(line).expect("parse fixture"));
        }
        app
    }

    fn screen(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render(frame, app))
            .expect("render frame");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let mut line = String::new();
                for x in 0..width {
                    line.push_str(buffer[(x, y)].symbol());
                }
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tree_rollup_badge_explains_failure_count() {
        let app = App::new(true, "test", None, Some("/omnifs".into()));
        let row = RenderRow {
            depth: 0,
            name: "github".to_string(),
            path: String::new(),
            mount: "github".to_string(),
            status: NodeStatus::Cached,
            is_subtree_handoff: false,
            last_latency_us: None,
            in_flight: 0,
            errors_below: 12,
        };
        let line = tree_row_line(&app, &row);
        assert!(line.to_string().contains("12 failed paths"));
    }

    #[test]
    fn recorded_journeys_have_stable_frames_and_semantic_cues() {
        let empty = App::new(true, "empty", None, Some("/omnifs/github".into()));
        let empty_screen = screen(&empty, 100, 28);
        assert!(empty_screen.contains("Nothing yet."));
        assert!(empty_screen.contains("ls /omnifs/github"));
        insta::assert_snapshot!("inspector_empty", empty_screen);

        let mut streaming = fixture_app();
        let streaming_screen = screen(&streaming, 100, 28);
        assert!(streaming_screen.contains("12:00:00.000"));
        assert!(streaming_screen.contains("trace 1"));
        assert!(streaming_screen.contains("220.0ms"));
        insta::assert_snapshot!("inspector_streaming", streaming_screen);

        streaming.filter.mode = FilterMode::ErrorsOnly;
        streaming.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(' '),
            crossterm::event::KeyModifiers::NONE,
        ));
        streaming.help_open = true;
        let filtered_screen = screen(&streaming, 100, 28);
        assert!(filtered_screen.contains("errors-only"));
        assert!(filtered_screen.contains("keys · ?/esc close"));
        assert!(filtered_screen.contains("paused"));
        insta::assert_snapshot!("inspector_filter_pause_help", filtered_screen);

        let mut disconnected = App::new(
            false,
            "daemon",
            Some("unix:/tmp/omnifs.sock".into()),
            Some("/omnifs/github".into()),
        );
        disconnected.apply_source_message(SourceMessage::Connected {
            epoch: "one".into(),
        });
        for line in include_str!("../tests/fixtures/inspector_journey.jsonl").lines() {
            disconnected.apply_line(InspectorLine::parse_line(line).expect("parse fixture"));
        }
        disconnected.apply_source_message(SourceMessage::Disconnected);
        let disconnected_screen = screen(&disconnected, 100, 28);
        assert!(disconnected_screen.contains("waiting on unix:/tmp/omnifs.sock"));
        assert!(disconnected_screen.contains("ENG-42"));
        insta::assert_snapshot!("inspector_disconnected", disconnected_screen);

        let compact_screen = screen(&fixture_app(), 72, 18);
        assert!(compact_screen.contains("operations"));
        assert!(!compact_screen.contains("recent paths"));
        insta::assert_snapshot!("inspector_compact", compact_screen);
    }
}
