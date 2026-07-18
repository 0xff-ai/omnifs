//! TUI state: operation store, mount windows, filters.

use std::collections::{HashSet, VecDeque};

use omnifs_api::events::{InspectorEvent, InspectorLine, InspectorRecord, TraceId};

use super::filter::{FilterMode, ViewFilter};
use super::metrics::MountWindow;
use super::source::SourceMessage;
use super::trace_state::{MAX_RECENT_TRACES, MountPalette, Operation, TraceReducer};
use super::tree::{ACTIVE_FOCUS_WINDOW_US, MountForest};

const EVENT_WINDOW: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionMode {
    Inspector,
    Replay,
}

pub struct App {
    pub mode: ConnectionMode,
    pub container: String,
    /// True only after the source thread reports at least one
    /// successful TCP connect. Stays false through every silent
    /// reconnect attempt so the header never lies about reachability.
    pub connected: bool,
    /// Inspector address shown in the header while disconnected.
    /// `None` in [`ConnectionMode::Replay`].
    pub addr: Option<String>,
    pub paused: bool,
    pub filter: ViewFilter,
    pub focus: PaneFocus,
    pub tree_cursor: Option<TreeCursor>,
    pub now_mono: u64,
    pub quit: bool,
    pub dropped_events: u64,
    pub events_per_sec: f64,
    /// Currently highlighted trace. View state, not fold state: a later
    /// time-travel slice refolds `TraceReducer` from scratch, so this
    /// must survive independently of the fold.
    selected: Option<TraceId>,
    /// Manually collapsed tree nodes, keyed by (mount, mount-relative
    /// path). Also view state for the same reason `selected` is.
    pub collapsed: HashSet<(String, String)>,
    traces: TraceReducer,
    event_times: VecDeque<u64>,
}

/// Which pane has keyboard focus. Tab cycles; arrow keys dispatch
/// against the focused pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaneFocus {
    #[default]
    Tree,
    OpsLog,
}

impl PaneFocus {
    fn cycle(self) -> Self {
        match self {
            Self::Tree => Self::OpsLog,
            Self::OpsLog => Self::Tree,
        }
    }
}

/// Path-keyed cursor that survives tree rerenders. We don't index by
/// row because rows shuffle as activity arrives — `(mount, path)` is
/// the stable identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeCursor {
    pub mount: String,
    pub path: String,
}

impl TreeCursor {
    /// Locate this cursor in `rows`; if absent, fall back to its deepest
    /// visible ancestor, then the mount root, then row zero.
    fn locate_or_nearest_ancestor(&self, rows: &[super::tree::RenderRow]) -> usize {
        if let Some(index) = rows
            .iter()
            .position(|row| row.mount == self.mount && row.path == self.path)
        {
            return index;
        }
        let mut probe = self.path.as_str();
        while let Some(slash) = probe.rfind('/') {
            probe = &probe[..slash];
            if let Some(index) = rows
                .iter()
                .position(|row| row.mount == self.mount && row.path == probe)
            {
                return index;
            }
        }
        rows.iter()
            .position(|row| row.mount == self.mount && row.path.is_empty())
            .unwrap_or(0)
    }
}

impl App {
    pub fn new(mode: ConnectionMode, container: impl Into<String>, addr: Option<String>) -> Self {
        Self {
            mode,
            container: container.into(),
            // Start false in Inspector mode: connection state is only
            // honest once the source thread actually attaches. Replay
            // mode ignores this flag in the header.
            connected: false,
            addr,
            paused: false,
            filter: ViewFilter::default(),
            focus: PaneFocus::default(),
            tree_cursor: None,
            now_mono: 0,
            quit: false,
            dropped_events: 0,
            events_per_sec: 0.0,
            selected: None,
            collapsed: HashSet::new(),
            traces: TraceReducer::default(),
            event_times: VecDeque::new(),
        }
    }

    pub fn forest(&self) -> &MountForest {
        &self.traces.forest
    }

    pub fn palette(&self) -> &MountPalette {
        &self.traces.palette
    }

    pub fn selected_trace(&self) -> Option<TraceId> {
        self.selected
    }

    pub fn mount_window(&self, mount: &str) -> Option<&MountWindow> {
        self.traces.mount_window(mount)
    }

    pub fn ordered_mounts_for_strip(&self, cap: usize) -> Vec<String> {
        self.traces.ordered_mounts_for_strip(cap)
    }

    pub fn operation(&self, trace_id: TraceId) -> Option<&Operation> {
        self.traces.operation(trace_id)
    }

    pub fn visible_trace_ids(&self) -> Vec<TraceId> {
        self.traces.visible_trace_ids(&self.filter)
    }

    /// Number of operations currently retained in memory. Pairs with
    /// [`MAX_RECENT_TRACES`] so subscribers can show eviction pressure.
    pub fn retained_trace_count(&self) -> usize {
        self.traces.retained_trace_count()
    }

    pub const fn max_retained_traces() -> usize {
        MAX_RECENT_TRACES
    }

    pub fn apply_record(&mut self, record: &InspectorRecord) {
        if self.paused {
            return;
        }
        self.now_mono = record.mono_us;
        self.tick_event_window(record.mono_us);
        self.traces.apply_record(record);

        // The fold evicts the oldest retained trace once MAX_RECENT_TRACES
        // is exceeded; it no longer owns selection, so a pointer to a
        // just-evicted trace has to be caught and cleared here.
        if self
            .selected
            .is_some_and(|id| self.traces.operation(id).is_none())
        {
            self.selected = None;
        }

        // A fresh FuseStart claims the initial selection so the very
        // first operation the user sees is highlighted without a keypress.
        if self.selected.is_none()
            && let InspectorEvent::FuseStart { .. } = &record.event
        {
            self.selected = Some(record.trace_id);
        }

        self.ensure_selected_visible();
    }

    /// Reassign selection when it points at a trace that's no longer
    /// visible: evicted from the fold, or filtered out by the active
    /// [`ViewFilter`]. Falls back to the first currently visible trace.
    fn ensure_selected_visible(&mut self) {
        let selected_is_visible = self.selected.is_some_and(|id| {
            self.traces
                .operation(id)
                .is_some_and(|op| self.filter.matches(op))
        });
        if !selected_is_visible {
            self.selected = self.traces.visible_trace_ids(&self.filter).first().copied();
        }
    }

    pub fn apply_line(&mut self, line: &InspectorLine) {
        match line {
            InspectorLine::Record(record) => self.apply_record(record),
            InspectorLine::Dropped { count } => {
                self.dropped_events = self.dropped_events.saturating_add(*count);
            },
        }
    }

    /// Consume one source message: line payload or a connection-state
    /// transition. Pairs with [`super::source::EventSource::drain`].
    pub fn apply_source_message(&mut self, message: SourceMessage) {
        match message {
            SourceMessage::Line(line) => {
                self.apply_line(&line);
            },
            SourceMessage::Connected => {
                self.connected = true;
            },
            SourceMessage::Disconnected => {
                self.connected = false;
            },
            SourceMessage::Finished | SourceMessage::Failed(_) => {},
        }
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        if self.filter.editing {
            self.handle_filter_key(key.code);
            return;
        }

        match key.code {
            KeyCode::Char('q' | 'c') | KeyCode::Esc
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.quit = true;
            },
            KeyCode::Tab => self.focus = self.focus.cycle(),
            KeyCode::Char(' ') => self.paused = !self.paused,
            KeyCode::Up => self.move_focus_cursor(-1),
            KeyCode::Down => self.move_focus_cursor(1),
            KeyCode::Enter if self.focus == PaneFocus::Tree => {
                self.toggle_tree_cursor_collapse();
            },
            KeyCode::Char('j' | 'n') => self.select_next(),
            KeyCode::Char('k' | 'p') => self.select_prev(),
            KeyCode::Char('e') => {
                self.filter.mode = match self.filter.mode {
                    FilterMode::ErrorsOnly => FilterMode::All,
                    FilterMode::All => FilterMode::ErrorsOnly,
                };
                self.ensure_selected_visible();
            },
            KeyCode::Char('r') => self.reset_recent(),
            KeyCode::Char('/') => {
                self.filter.editing = true;
                self.filter.query.clear();
            },
            _ => {},
        }
    }

    fn move_focus_cursor(&mut self, delta: isize) {
        match self.focus {
            PaneFocus::Tree => self.move_tree_cursor(delta),
            PaneFocus::OpsLog => {
                if delta > 0 {
                    self.select_next();
                } else {
                    self.select_prev();
                }
            },
        }
    }

    fn move_tree_cursor(&mut self, delta: isize) {
        let rows =
            self.forest()
                .render_rows(self.now_mono, ACTIVE_FOCUS_WINDOW_US, &self.collapsed);
        if rows.is_empty() {
            self.tree_cursor = None;
            return;
        }
        let last = rows.len() - 1;
        let new_idx = match self.tree_cursor.as_ref() {
            Some(cursor) => {
                // If the stored path was pruned by a collapse or active-focus
                // eviction, fall back to its nearest visible ancestor (or
                // the mount root) so Down doesn't silently teleport to row 0.
                let current = cursor.locate_or_nearest_ancestor(&rows);
                step_clamped(current, delta, last)
            },
            None if delta < 0 => last,
            None => 0,
        };
        let row = &rows[new_idx];
        self.tree_cursor = Some(TreeCursor {
            mount: row.mount.clone(),
            path: row.path.clone(),
        });
        self.sync_selection_to_tree_cursor();
    }

    fn sync_selection_to_tree_cursor(&mut self) {
        let Some(cursor) = self.tree_cursor.clone() else {
            return;
        };
        self.select_latest_for_path(&cursor.mount, &cursor.path);
    }

    /// Select the most recently active operation at or below `mount`/`path`,
    /// so moving the tree cursor onto a node also highlights the operations
    /// log entry the user is looking at. Considers every retained trace
    /// (not just the currently filtered-visible ones), matching the old
    /// reducer-owned behavior of scanning all operations.
    fn select_latest_for_path(&mut self, mount: &str, path: &str) {
        let mut best: Option<(u64, TraceId)> = None;
        for id in self.traces.visible_trace_ids(&ViewFilter::default()) {
            let Some(op) = self.traces.operation(id) else {
                continue;
            };
            if op.mount != mount {
                continue;
            }
            let matches_path =
                path.is_empty() || op.path == path || op.path.starts_with(&format!("{path}/"));
            if !matches_path {
                continue;
            }
            let ts = op.ended_mono.unwrap_or(op.started_mono);
            if best.is_none_or(|(prev, _)| ts >= prev) {
                best = Some((ts, id));
            }
        }
        if let Some((_, trace_id)) = best {
            self.selected = Some(trace_id);
        }
    }

    fn toggle_tree_cursor_collapse(&mut self) {
        let Some(cursor) = self.tree_cursor.clone() else {
            return;
        };
        let key = (cursor.mount, cursor.path);
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
    }

    fn handle_filter_key(&mut self, code: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Esc => {
                self.filter.editing = false;
                self.filter.query.clear();
            },
            KeyCode::Enter => self.filter.editing = false,
            KeyCode::Backspace => {
                self.filter.query.pop();
            },
            KeyCode::Char(ch) => self.filter.query.push(ch),
            _ => {},
        }
    }

    fn tick_event_window(&mut self, mono_us: u64) {
        self.event_times.push_back(mono_us);
        while self.event_times.len() > EVENT_WINDOW {
            self.event_times.pop_front();
        }
        self.recompute_event_rate(mono_us);
    }

    // Event-rate math intentionally uses f64. Precision loss across
    // event_times.len() (capped at EVENT_WINDOW) and microsecond
    // intervals is irrelevant for a UI counter.
    #[allow(clippy::cast_precision_loss, clippy::similar_names)]
    fn recompute_event_rate(&mut self, mono_us: u64) {
        if self.event_times.len() < 2 {
            return;
        }
        let oldest = self.event_times.front().copied().unwrap_or(mono_us);
        let interval_us = mono_us.saturating_sub(oldest);
        if interval_us == 0 {
            return;
        }
        let interval_seconds = interval_us as f64 / 1_000_000.0;
        self.events_per_sec = (self.event_times.len() - 1) as f64 / interval_seconds;
    }

    fn reset_recent(&mut self) {
        self.traces.reset_recent();
        self.ensure_selected_visible();
    }

    fn select_next(&mut self) {
        let visible = self.traces.visible_trace_ids(&self.filter);
        if visible.is_empty() {
            return;
        }
        let idx = self
            .selected
            .and_then(|sel| visible.iter().position(|id| *id == sel))
            .map_or(0, |i| (i + 1).min(visible.len() - 1));
        self.selected = Some(visible[idx]);
    }

    fn select_prev(&mut self) {
        let visible = self.traces.visible_trace_ids(&self.filter);
        if visible.is_empty() {
            return;
        }
        let idx = self
            .selected
            .and_then(|sel| visible.iter().position(|id| *id == sel))
            .map_or(0, |i| i.saturating_sub(1));
        self.selected = Some(visible[idx]);
    }
}

/// Apply `delta` to `current`, clamped to `[0, max_inclusive]`. Handles
/// negative deltas without ever going through `isize` math on possibly-
/// huge `usize`s.
fn step_clamped(current: usize, delta: isize, max_inclusive: usize) -> usize {
    if delta >= 0 {
        let step = delta.unsigned_abs();
        current.saturating_add(step).min(max_inclusive)
    } else {
        current.saturating_sub(delta.unsigned_abs())
    }
}
