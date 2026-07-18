//! The sandbox map ("patch bay") view: one mount's exported ports (host
//! invokes guest) and imported ports (guest awaits host) rendered
//! either side of its wasm sandbox box, plus a scrub bar mirroring the
//! activity view's time travel.
//!
//! Like the activity view, every stat here reads through [`App`]'s
//! view accessors (`mount_sandbox`, `view_now_mono`, ...), so pausing
//! and scrubbing cover the sandbox map for free.

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
use super::sandbox::{self, MountSandbox, PortId};
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
    render_body(frame, app, chunks[1]);
}

fn render_body(frame: &mut Frame, app: &App, area: Rect) {
    let Some(mount) = app.sandbox_active_mount() else {
        render_empty_state(frame, app, area);
        return;
    };
    let sandbox = app.mount_sandbox(&mount);

    let chunks = Layout::vertical([
        Constraint::Length(1), // mount strip
        Constraint::Min(5),    // rails + box
        Constraint::Length(1), // pinned footer
        Constraint::Length(1), // scrub bar
    ])
    .split(area);

    render_mount_strip(frame, app, chunks[0], &mount);
    render_rails(frame, app, chunks[1], &mount, sandbox);
    render_pinned_footer(frame, app, chunks[2], sandbox);
    render_scrub_bar(frame, app, chunks[3]);
}

fn render_empty_state(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(area);
    let msg = Paragraph::new("no provider activity yet")
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(msg, chunks[0]);
    render_scrub_bar(frame, app, chunks[1]);
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
    sandbox: Option<&MountSandbox>,
) {
    let exports = sandbox::export_port_ids(sandbox);
    let imports = sandbox::import_port_ids(sandbox);
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
        render_port_column(
            frame,
            app,
            chunks[0],
            sandbox,
            &exports,
            PortDirection::Export,
            show_sparkline,
        );
        render_box(frame, app, chunks[1], mount, sandbox);
        render_port_column(
            frame,
            app,
            chunks[2],
            sandbox,
            &imports,
            PortDirection::Import,
            show_sparkline,
        );
        return;
    }

    let col_width = area.width.saturating_sub(BOX_WIDTH) / 2;
    let chunks = Layout::horizontal([
        Constraint::Length(col_width),
        Constraint::Length(BOX_WIDTH),
        Constraint::Min(0),
    ])
    .split(area);
    render_port_column(
        frame,
        app,
        chunks[0],
        sandbox,
        &exports,
        PortDirection::Export,
        show_sparkline,
    );
    render_box(frame, app, chunks[1], mount, sandbox);
    render_port_column(
        frame,
        app,
        chunks[2],
        sandbox,
        &imports,
        PortDirection::Import,
        show_sparkline,
    );
}

/// One column of port rows, oriented by `direction`: exports point into
/// the box, imports point out of it.
fn render_port_column(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    sandbox: Option<&MountSandbox>,
    ports: &[PortId],
    direction: PortDirection,
    show_sparkline: bool,
) {
    let lines: Vec<Line<'static>> = ports
        .iter()
        .map(|port| port_row_line(app, sandbox, port, direction, show_sparkline))
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
    sandbox: Option<&MountSandbox>,
) {
    let color = app.palette().peek(mount).unwrap_or(Color::White);
    let title = sandbox
        .map(|s| s.provider.clone())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| mount.to_string());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(format!(" {title} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let in_flight = sandbox.map_or(0, MountSandbox::total_open_exports);
    let callouts_open = sandbox.map_or(0, MountSandbox::total_open_imports);
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

fn render_pinned_footer(frame: &mut Frame, app: &App, area: Rect, sandbox: Option<&MountSandbox>) {
    if !app.port_pinned {
        return;
    }
    let Some(port) = app.port_cursor.clone() else {
        return;
    };
    let line = Line::from(vec![
        Span::styled(
            "▸ pinned: ",
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(port_label(&port)),
        Span::raw("   "),
        Span::styled(
            pinned_detail(app, sandbox, &port),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn port_label(port: &PortId) -> String {
    match port {
        PortId::Export(method) => dashed(method),
        PortId::Import(kind) => dashed(kind.as_str()),
        PortId::Log => "log".to_string(),
    }
}

/// The pinned footer's detail text: for an export, the newest matching
/// operation's path/outcome/elapsed; for an import, an in-flight
/// callout's live summary if one is open, else the port's lifetime
/// count and p95 (there's no per-kind "last completed" detail to show,
/// since callouts aren't retained past their own operation); for Log,
/// there's nothing to show at all.
fn pinned_detail(app: &App, sandbox: Option<&MountSandbox>, port: &PortId) -> String {
    match port {
        PortId::Export(method) => {
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
        PortId::Import(kind) => {
            let Some(sandbox) = sandbox else {
                return "no activity".to_string();
            };
            if let Some((_, summary, start_mono)) =
                sandbox.open_import_calls().find(|(k, _, _)| *k == *kind)
            {
                let elapsed = app.view_now_mono().saturating_sub(start_mono);
                format!("{summary}  running {}", format::format_latency_us(elapsed))
            } else {
                let p95 = sandbox
                    .import_window(*kind)
                    .and_then(MountWindow::p95_latency_us)
                    .map_or_else(|| "—".to_string(), format::format_latency_us);
                let count = sandbox.import_lifetime_count(*kind);
                format!("{count} calls  p95 {p95}")
            }
        },
        PortId::Log => "untraced".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortDirection {
    Export,
    Import,
}

/// WIT-style dashed display name from a wire method/kind string, e.g.
/// `lookup_child` -> `lookup-child`. One mapping so every port label
/// (export methods and import kinds alike) uses the same rule.
fn dashed(wire: &str) -> String {
    wire.replace('_', "-")
}

/// Per-port fields the row renderer needs, resolved once per row so
/// the rendering match arms don't each re-derive them.
struct PortStats<'a> {
    label: String,
    window: Option<&'a MountWindow>,
    open_now: bool,
    lifetime: u64,
    untraced: bool,
}

fn port_stats<'a>(sandbox: Option<&'a MountSandbox>, port: &PortId) -> PortStats<'a> {
    match port {
        PortId::Export(method) => PortStats {
            label: dashed(method),
            window: sandbox.and_then(|s| s.export_window(method)),
            open_now: sandbox.is_some_and(|s| s.export_open_count(method) > 0),
            lifetime: sandbox.map_or(0, |s| s.export_lifetime_count(method)),
            untraced: sandbox::UNTRACED_EXPORTS.contains(&method.as_str()),
        },
        PortId::Import(kind) => PortStats {
            label: dashed(kind.as_str()),
            window: sandbox.and_then(|s| s.import_window(*kind)),
            open_now: sandbox.is_some_and(|s| s.import_open_count(*kind) > 0),
            lifetime: sandbox.map_or(0, |s| s.import_lifetime_count(*kind)),
            untraced: false,
        },
        PortId::Log => PortStats {
            label: "log".to_string(),
            window: None,
            open_now: false,
            lifetime: 0,
            untraced: true,
        },
    }
}

/// The wire-tier glyph for one port row, oriented for its direction:
/// export wires point into the sandbox box, import wires point out of
/// it. One function producing both orientations so the four tiers
/// (hot / warm / idle / untraced) are only ever defined in one place.
fn wire_glyph(
    direction: PortDirection,
    open_now: bool,
    has_samples: bool,
    lifetime_count: u64,
    untraced: bool,
) -> (&'static str, Color) {
    if untraced {
        return ("····", Color::DarkGray);
    }
    if open_now {
        return (
            match direction {
                PortDirection::Export => "●══▶",
                PortDirection::Import => "══▶●",
            },
            Color::LightGreen,
        );
    }
    if has_samples {
        return (
            match direction {
                PortDirection::Export => "○──▶",
                PortDirection::Import => "──▶○",
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
    sandbox: Option<&MountSandbox>,
    port: &PortId,
    direction: PortDirection,
    show_sparkline: bool,
) -> Line<'static> {
    let now_mono = app.view_now_mono();
    let is_cursor = app.port_cursor.as_ref() == Some(port);
    let stats = port_stats(sandbox, port);
    let has_samples = stats.window.is_some_and(|w| !w.is_empty());
    let (wire, wire_color) = wire_glyph(
        direction,
        stats.open_now,
        has_samples,
        stats.lifetime,
        stats.untraced,
    );
    let text_color = if wire_color == Color::DarkGray {
        Color::DarkGray
    } else {
        Color::White
    };
    let label_span = Span::styled(
        format!("{:<LABEL_WIDTH$}", stats.label),
        Style::default().fg(text_color),
    );
    let wire_span = Span::styled(wire, Style::default().fg(wire_color));

    let mut spans = Vec::new();
    if stats.untraced {
        let tag = Span::styled("untraced", Style::default().fg(Color::DarkGray));
        match direction {
            PortDirection::Export => {
                spans.extend([label_span, Span::raw("  "), tag, Span::raw("  "), wire_span]);
            },
            PortDirection::Import => {
                spans.extend([wire_span, Span::raw("  "), label_span, Span::raw("  "), tag]);
            },
        }
    } else {
        let count = Span::raw(format!("{:>5}", stats.lifetime));
        let p95_text = stats
            .window
            .and_then(MountWindow::p95_latency_us)
            .map_or_else(|| "—".to_string(), format::format_latency_us);
        let p95 = Span::raw(format!("{p95_text:>7}"));
        let bars = if show_sparkline {
            if has_samples {
                render_sparkline(
                    &stats
                        .window
                        .map_or_else(Vec::new, |w| w.sparkline(now_mono, SPARK_BUCKETS)),
                )
            } else {
                " ".repeat(SPARK_BUCKETS)
            }
        } else {
            String::new()
        };
        let bars_span = Span::styled(bars, Style::default().fg(wire_color));
        match direction {
            PortDirection::Export => {
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
            },
            PortDirection::Import => {
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
            },
        }
    }

    let mut line = Line::from(spans);
    if is_cursor {
        line = line.patch_style(Style::default().bg(ui::CURSOR_BG));
    }
    line
}

/// Live/paused scrub bar, bottom line of the sandbox map. Mirrors the
/// activity view's pause affordance so the same mental model (space to
/// pause, ←/→ to step, g to go live) applies to both screens.
fn render_scrub_bar(frame: &mut Frame, app: &App, area: Rect) {
    let line = if app.paused() {
        paused_scrub_line(app)
    } else {
        live_scrub_line(app)
    };
    frame.render_widget(Paragraph::new(line), area);
}

#[allow(clippy::cast_precision_loss)]
fn format_span_us(us: u64) -> String {
    let secs = us as f64 / 1_000_000.0;
    if secs >= 60.0 {
        let mins = (secs / 60.0).floor();
        let rem = secs - mins * 60.0;
        format!("{mins:.0}m{rem:02.0}s")
    } else {
        format!("{secs:.1}s")
    }
}

fn live_scrub_line(app: &App) -> Line<'static> {
    let retained = app.timeline_retained_count();
    let span_text = app.timeline_oldest_mono_us().map_or_else(
        || "0s".to_string(),
        |oldest| format_span_us(app.now_mono.saturating_sub(oldest)),
    );
    Line::from(vec![
        Span::styled("  ● live", Style::default().fg(Color::LightGreen)),
        Span::raw(format!("  buffered {span_text}  {retained} records")),
        Span::styled(
            "   space pause  ←/→ step  g live",
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

fn paused_scrub_line(app: &App) -> Line<'static> {
    let delta_text = format_span_us(app.now_mono.saturating_sub(app.view_now_mono()));
    let oldest = app
        .timeline_oldest_mono_us()
        .unwrap_or_else(|| app.view_now_mono());
    let track = scrub_track(oldest, app.now_mono, app.view_now_mono(), 20);
    Line::from(vec![
        Span::styled("  ⏸ paused", Style::default().fg(Color::LightYellow)),
        Span::raw(format!(" at −{delta_text}  [{track}]")),
        Span::styled(
            "  space resume  ←/→ step  g live",
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

/// Fraction (0.0-1.0) of the way `cursor` sits between `oldest` and
/// `now`, clamped to that range. Pulled out as its own function so the
/// scrub math is unit-testable independent of rendering.
#[allow(clippy::cast_precision_loss)]
fn scrub_fraction(oldest: u64, now: u64, cursor: u64) -> f64 {
    if now <= oldest {
        return 0.0;
    }
    let span = (now - oldest) as f64;
    let pos = cursor.saturating_sub(oldest) as f64;
    (pos / span).clamp(0.0, 1.0)
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn scrub_track(oldest: u64, now: u64, cursor: u64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let fraction = scrub_fraction(oldest, now, cursor);
    let marker = ((fraction * (width - 1) as f64).round() as usize).min(width - 1);
    (0..width)
        .map(|i| if i == marker { '▓' } else { '░' })
        .collect()
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
        let mut app = App::new(ConnectionMode::Replay, "test", None);
        app.apply_record(provider_start(1, 10, "github", "lookup_child"));

        let sandbox = app.mount_sandbox("github");
        let line = port_row_line(
            &app,
            sandbox,
            &PortId::Export("lookup_child".to_string()),
            PortDirection::Export,
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
        let app = App::new(ConnectionMode::Replay, "test", None);
        let line = port_row_line(
            &app,
            None,
            &PortId::Export("initialize".to_string()),
            PortDirection::Export,
            true,
        );
        let text = line.to_string();
        assert!(text.contains("untraced"));
        assert!(text.contains("····"));
    }

    #[test]
    fn log_pseudo_port_is_always_untraced() {
        let app = App::new(ConnectionMode::Replay, "test", None);
        let line = port_row_line(&app, None, &PortId::Log, PortDirection::Import, true);
        let text = line.to_string();
        assert!(text.contains("untraced"));
        assert!(text.contains("log"));
    }

    #[test]
    fn scrub_fraction_reflects_cursor_position_between_oldest_and_now() {
        assert!((scrub_fraction(0, 100, 50) - 0.5).abs() < 1e-9);
        assert!((scrub_fraction(0, 100, 0) - 0.0).abs() < 1e-9);
        assert!((scrub_fraction(0, 100, 100) - 1.0).abs() < 1e-9);
        // Degenerate range (nothing retained yet, or now hasn't moved
        // past oldest): never divide by zero or go negative.
        assert!((scrub_fraction(50, 50, 25) - 0.0).abs() < 1e-9);
    }
}
