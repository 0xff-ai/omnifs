//! The sandbox map ("patch bay") view: one mount's exported ports (host
//! invokes guest) and imported ports (guest awaits host) rendered
//! either side of its wasm sandbox box.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use super::app::App;
use super::format;
use super::metrics::{MountWindow, render_sparkline};
use super::sandbox::{MountSandboxView, PortId};
use super::ui;

/// 12-bucket sparkline per port row, matching the activity view's
/// per-mount strip granularity.
const SPARK_BUCKETS: usize = 12;
/// Below this inner width, drop the sparkline column entirely; there's
/// no room for it next to the name/count/p95 cells.
const NARROW_WIDTH: u16 = 90;
/// Below this inner width, stack exports/box/imports vertically instead
/// of side by side; there's no room for three columns at all.
const STACK_WIDTH: u16 = 70;
const BOX_WIDTH: u16 = 28;
/// Fixed sandbox-box interior lines: "wasm32-wasip2", "in flight N",
/// "callouts open N", "cache hit NN%", "errors N%".
const BOX_INTERIOR_LINES: u16 = 5;
/// Widest dashed port label ("list-children" / "git-open-repo").
const LABEL_WIDTH: usize = 14;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(6)]).split(area);
    ui::render_header(frame, app, chunks[0]);
    render_map(frame, app, chunks[1]);
}

fn render_map(frame: &mut Frame, app: &App, area: Rect) {
    let Some(mount) = app.sandbox_active_mount() else {
        render_empty_state(frame, area);
        return;
    };
    let sandbox = app.mount_sandbox(mount);

    let chunks = Layout::vertical([
        Constraint::Length(1), // mount strip
        Constraint::Min(5),    // rails + box
        Constraint::Length(1), // selected-port detail
    ])
    .split(area);

    render_mount_strip(frame, app, chunks[0], mount);
    render_rails(frame, app, chunks[1], mount, sandbox.as_ref());
    render_selected_footer(frame, app, chunks[2], sandbox.as_ref());
}

fn render_empty_state(frame: &mut Frame, area: Rect) {
    let msg = Paragraph::new("no provider activity yet")
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(msg, area);
}

/// One line above the rails listing sandbox mounts by activity, active
/// one highlighted, so `m` cycling is visible even before the cursor
/// touches a port.
fn render_mount_strip(frame: &mut Frame, app: &App, area: Rect, active_mount: &str) {
    let mounts = app.sandbox_mounts_by_activity();
    let mut spans = vec![Span::styled(
        "  mounts  ",
        Style::default().fg(Color::DarkGray),
    )];
    for (index, mount) in mounts.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let color = app.palette().peek(mount).unwrap_or(Color::White);
        let style = if *mount == active_mount {
            Style::default()
                .fg(Color::Black)
                .bg(color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color)
        };
        spans.push(Span::styled((*mount).to_string(), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_rails(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    mount: &str,
    sandbox: Option<&MountSandboxView<'_>>,
) {
    let exports = PortId::exports();
    let imports = PortId::imports();
    let show_sparkline = area.width >= NARROW_WIDTH;

    if area.width < STACK_WIDTH {
        let export_h = u16::try_from(exports.len()).unwrap_or(u16::MAX);
        let import_h = u16::try_from(imports.len()).unwrap_or(u16::MAX);
        let chunks = Layout::vertical([
            Constraint::Length(export_h),
            Constraint::Length(BOX_INTERIOR_LINES + 2),
            Constraint::Length(import_h),
        ])
        .split(area);
        render_port_column(frame, app, chunks[0], sandbox, &exports, show_sparkline);
        render_box(frame, app, chunks[1], mount, sandbox);
        render_port_column(frame, app, chunks[2], sandbox, &imports, show_sparkline);
        return;
    }

    let col_width = area.width.saturating_sub(BOX_WIDTH) / 2;
    let chunks = Layout::horizontal([
        Constraint::Length(col_width),
        Constraint::Length(BOX_WIDTH),
        Constraint::Min(0),
    ])
    .split(area);
    render_port_column(frame, app, chunks[0], sandbox, &exports, show_sparkline);
    render_box(frame, app, chunks[1], mount, sandbox);
    render_port_column(frame, app, chunks[2], sandbox, &imports, show_sparkline);
}

/// One column of port rows. Export wires point into the box; import
/// wires point out of it.
fn render_port_column(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    sandbox: Option<&MountSandboxView<'_>>,
    ports: &[PortId],
    show_sparkline: bool,
) {
    let lines: Vec<Line<'static>> = ports
        .iter()
        .map(|port| port_row_line(app, sandbox, port, show_sparkline))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// The bordered sandbox box: provider name as the title, mount-level
/// stats as interior lines, vertically centered so the box reads well
/// whether it's squat (stacked layout) or tall (matching the exports
/// rail's height in the side-by-side layout).
fn render_box(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    mount: &str,
    sandbox: Option<&MountSandboxView<'_>>,
) {
    let color = app.palette().peek(mount).unwrap_or(Color::White);
    let title = sandbox
        .map(|s| s.provider().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| mount.to_string());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(format!(" {title} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let in_flight = sandbox.map_or(0, MountSandboxView::total_open_exports);
    let callouts_open = sandbox.map_or(0, MountSandboxView::total_open_imports);
    let mount_window = app.mount_window(mount);
    let cache = mount_window
        .and_then(MountWindow::cache_hit_ratio)
        .map_or_else(|| "—".to_string(), |ratio| format!("{:.0}%", ratio * 100.0));
    let errors = mount_window.map_or(0.0, MountWindow::error_rate);

    let content = vec![
        Line::styled("wasm32-wasip2", Style::default().fg(Color::DarkGray)),
        Line::styled(format!("in flight {in_flight}"), Style::default().fg(color)),
        Line::styled(
            format!("callouts open {callouts_open}"),
            Style::default().fg(color),
        ),
        Line::styled(format!("cache hit {cache}"), Style::default().fg(color)),
        Line::styled(
            format!("errors {:.1}%", errors * 100.0),
            Style::default().fg(color),
        ),
    ];
    let pad_top = inner.height.saturating_sub(BOX_INTERIOR_LINES) / 2;
    let mut lines = Vec::with_capacity(inner.height as usize);
    for _ in 0..pad_top {
        lines.push(Line::raw(""));
    }
    lines.extend(content);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_selected_footer(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    sandbox: Option<&MountSandboxView<'_>>,
) {
    let Some(selection) = app.sandbox.selection.as_ref() else {
        return;
    };
    let port = selection;
    let line = Line::from(vec![
        Span::styled(
            "▸ selected: ",
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(port.label()),
        Span::raw("   "),
        Span::styled(
            selected_detail(app, sandbox, port),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// The selected footer's detail text: for an export, an in-flight call's
/// running elapsed if one is open, else the newest matching operation's
/// path/outcome/elapsed; for an import, an in-flight callout's live
/// summary if one is open, else the port's lifetime count and p95
/// (there's no per-kind "last completed" detail to show, since callouts
/// aren't retained past their own operation).
fn selected_detail(app: &App, sandbox: Option<&MountSandboxView<'_>>, port: &PortId) -> String {
    match port {
        PortId::Export(method) => {
            if let Some(start_mono) = sandbox
                .and_then(|s| s.open_call(port))
                .map(|(start_mono, _)| start_mono)
            {
                let elapsed = app.view_now_mono().saturating_sub(start_mono);
                return format!("running {}", format::format_latency_us(elapsed));
            }
            let best = app
                .visible_trace_ids()
                .into_iter()
                .filter_map(|id| app.operation(id))
                .filter(|op| op.provider_method.as_deref() == Some(method.as_str()))
                .max_by_key(|op| op.ended_mono.unwrap_or(op.started_mono));
            match best {
                Some(op) => {
                    let outcome = op
                        .outcome
                        .map_or_else(|| "…".to_string(), |o| o.to_string());
                    let elapsed = op
                        .fuse_elapsed_us
                        .map_or_else(String::new, format::format_latency_us);
                    format!("last: {}  {outcome} {elapsed}", op.path)
                },
                None => "no recent calls".to_string(),
            }
        },
        PortId::Import(_) => {
            let Some(sandbox) = sandbox else {
                return "no activity".to_string();
            };
            if let Some((start_mono, summary)) = sandbox.open_call(port) {
                let elapsed = app.view_now_mono().saturating_sub(start_mono);
                let summary = summary.unwrap_or("callout");
                format!("{summary}  running {}", format::format_latency_us(elapsed))
            } else {
                let p95 = sandbox
                    .port_stats(port)
                    .map(|stats| &stats.window)
                    .and_then(MountWindow::p95_latency_us)
                    .map_or_else(|| "—".to_string(), format::format_latency_us);
                let count = sandbox.port_stats(port).map_or(0, |stats| stats.lifetime);
                format!("{count} calls  p95 {p95}")
            }
        },
    }
}

/// The wire-tier glyph for one port row, oriented for its direction:
/// export wires point into the sandbox box, import wires point out of
/// it. One function producing both orientations so the four tiers
/// (hot / warm / idle / untraced) are only ever defined in one place.
fn wire_glyph(
    port: &PortId,
    open_now: bool,
    has_samples: bool,
    lifetime_count: u64,
) -> (&'static str, Color) {
    if port.is_untraced() {
        return ("····", Color::DarkGray);
    }
    if open_now {
        return (
            if port.is_export() {
                "●══▶"
            } else {
                "══▶●"
            },
            Color::LightGreen,
        );
    }
    if has_samples {
        return (
            if port.is_export() {
                "○──▶"
            } else {
                "──▶○"
            },
            Color::Cyan,
        );
    }
    if lifetime_count > 0 {
        return ("───", Color::DarkGray);
    }
    ("····", Color::DarkGray)
}

fn port_row_line(
    app: &App,
    sandbox: Option<&MountSandboxView<'_>>,
    port: &PortId,
    show_sparkline: bool,
) -> Line<'static> {
    let now_mono = app.view_now_mono();
    let is_cursor = app
        .sandbox
        .selection
        .as_ref()
        .is_some_and(|selection| selection == port);
    let stats = sandbox.and_then(|sandbox| sandbox.port_stats(port));
    let window = stats.map(|stats| &stats.window);
    let open_now = sandbox.is_some_and(|sandbox| sandbox.open_count(port) > 0);
    let lifetime = stats.map_or(0, |stats| stats.lifetime);
    let has_samples = window.is_some_and(|w| !w.is_empty());
    let untraced = port.is_untraced();
    let export = port.is_export();
    let (wire, wire_color) = wire_glyph(port, open_now, has_samples, lifetime);
    let text_color = if wire_color == Color::DarkGray {
        Color::DarkGray
    } else {
        Color::White
    };
    let label_span = Span::styled(
        format!("{:<LABEL_WIDTH$}", port.label()),
        Style::default().fg(text_color),
    );
    let wire_span = Span::styled(wire, Style::default().fg(wire_color));

    let mut spans = Vec::new();
    if untraced {
        let tag = Span::styled("untraced", Style::default().fg(Color::DarkGray));
        if export {
            spans.extend([label_span, Span::raw("  "), tag, Span::raw("  "), wire_span]);
        } else {
            spans.extend([wire_span, Span::raw("  "), label_span, Span::raw("  "), tag]);
        }
    } else {
        let count = Span::raw(format!("{lifetime:>5}"));
        let p95_text = window
            .and_then(MountWindow::p95_latency_us)
            .map_or_else(|| "—".to_string(), format::format_latency_us);
        let p95 = Span::raw(format!("{p95_text:>7}"));
        let bars = if show_sparkline {
            if has_samples {
                render_sparkline(
                    &window.map_or_else(Vec::new, |w| w.sparkline(now_mono, SPARK_BUCKETS)),
                )
            } else {
                " ".repeat(SPARK_BUCKETS)
            }
        } else {
            String::new()
        };
        let bars_span = Span::styled(bars, Style::default().fg(wire_color));
        if export {
            spans.push(label_span);
            if show_sparkline {
                spans.push(Span::raw("  "));
                spans.push(bars_span);
            }
            spans.push(Span::raw("  "));
            spans.push(count);
            spans.push(Span::raw("  "));
            spans.push(p95);
            spans.push(Span::raw("  "));
            spans.push(wire_span);
        } else {
            spans.push(wire_span);
            spans.push(Span::raw("  "));
            spans.push(label_span);
            spans.push(Span::raw("  "));
            spans.push(count);
            spans.push(Span::raw("  "));
            spans.push(p95);
            if show_sparkline {
                spans.push(Span::raw("  "));
                spans.push(bars_span);
            }
        }
    }

    let mut line = Line::from(spans);
    if is_cursor {
        line = line.patch_style(Style::default().bg(ui::CURSOR_BG));
    }
    line
}

#[cfg(test)]
mod tests {
    use omnifs_api::events::{InspectorEvent, InspectorRecord, TraceId};

    use super::*;
    use crate::inspector::app::ConnectionMode;

    fn record(trace_id: TraceId, mono_us: u64, event: InspectorEvent) -> InspectorRecord {
        InspectorRecord::new("2026-05-23T12:00:00Z", mono_us, trace_id, event)
    }

    fn provider_start(
        trace_id: TraceId,
        mono_us: u64,
        mount: &str,
        method: &str,
    ) -> InspectorRecord {
        record(
            trace_id,
            mono_us,
            InspectorEvent::ProviderStart {
                operation_id: trace_id,
                mount: mount.into(),
                provider: mount.into(),
                method: method.into(),
                path: "/x".into(),
            },
        )
    }

    #[test]
    fn traced_port_with_open_call_renders_hot_wire_and_lifetime_count() {
        let mut app = App::new(ConnectionMode::Replay, "test", None, "/omnifs");
        app.apply_record(provider_start(1, 10, "github", "lookup_child"));

        let sandbox = app.mount_sandbox("github");
        let line = port_row_line(
            &app,
            sandbox.as_ref(),
            &PortId::Export("lookup_child".to_string()),
            true,
        );
        let text = line.to_string();
        assert!(text.contains("●══▶"), "expected hot wire glyph in {text:?}");
        assert!(
            text.contains("lookup-child"),
            "expected dashed label in {text:?}"
        );
        assert!(
            text.contains('1'),
            "expected the lifetime count in {text:?}"
        );
    }

    #[test]
    fn untraced_port_renders_the_untraced_tag_and_dotted_wire() {
        let app = App::new(ConnectionMode::Replay, "test", None, "/omnifs");
        let line = port_row_line(&app, None, &PortId::Export("initialize".to_string()), true);
        let text = line.to_string();
        assert!(text.contains("untraced"));
        assert!(text.contains("····"));
    }
}
