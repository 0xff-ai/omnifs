//! TUI state: operation store, mount windows, filters.

use std::collections::{HashSet, VecDeque};

use omnifs_api::events::{InspectorEvent, InspectorLine, InspectorRecord, TraceId};

use super::filter::{FilterMode, ViewFilter};
use super::metrics::MountWindow;
use super::sandbox::{self, MountSandboxView, PortId};
use super::source::SourceMessage;
use super::timeline::Timeline;
use super::trace_state::{MAX_RECENT_TRACES, MountPalette, Operation, TraceReducer};
use super::tree::{ACTIVE_FOCUS_WINDOW_US, MountForest};

const EVENT_WINDOW: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionMode {
    Inspector,
    Replay,
}

/// Which full-screen view the TUI is showing. `v` toggles between them
/// in either direction; both views share the header, the connection
/// state, and every scrub/filter control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppView {
    #[default]
    Activity,
    Sandbox,
}

impl AppView {
    fn toggle(self) -> Self {
        match self {
            Self::Activity => Self::Sandbox,
            Self::Sandbox => Self::Activity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SandboxMapState {
    pub active_mount: Option<String>,
    pub selection: Option<PortSelection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSelection {
    pub port: PortId,
    pub pinned: bool,
}

// This is a UI-state struct whose bools are independent toggles
// (connection observed by the source thread and quit signal), not an
// implicit state machine crying out for an enum.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    pub mode: ConnectionMode,
    pub container: String,
    /// True only after the source thread reports at least one
    /// successful TCP connect. Stays false through every silent
    /// reconnect attempt so the header never lies about reachability.
    pub connected: bool,
    daemon_epoch: Option<String>,
    /// Inspector address shown in the header while disconnected.
    /// `None` in [`ConnectionMode::Replay`].
    pub addr: Option<String>,
    pub filter: ViewFilter,
    pub focus: PaneFocus,
    pub tree_cursor: Option<TreeCursor>,
    /// Which full-screen view is active. Toggled by `v`.
    pub view: AppView,
    pub sandbox: SandboxMapState,
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
    /// Every record that has arrived, bounded and addressed by absolute
    /// ordinal. Backs pause/scrub; `traces` never reads from it directly.
    timeline: Timeline,
    /// `Some` while paused: a reducer frozen (or stepped) at some point
    /// in the timeline, read by every render/selection accessor instead
    /// of `traces`. `None` means live.
    scrub: Option<Scrub>,
}

/// A paused view: a `TraceReducer` folded up to `cursor`, an absolute
/// timeline ordinal one past the last folded record. Rebuilt by
/// refolding a prefix of the ring rather than trying to run the fold
/// backward, since `TraceReducer::apply_record` has no inverse.
struct Scrub {
    cursor: u64,
    reducer: TraceReducer,
}

impl Scrub {
    /// Fold the entire ring: the state a fresh pause freezes on.
    fn paused_at(timeline: &Timeline) -> Self {
        Self::at(timeline, timeline.end())
    }

    /// Rebuild from scratch by folding `ring[evicted..ordinal)`, clamped
    /// into the retained range. O(ring length), but this only runs on
    /// pause, backward steps, and second-granularity jumps, never once
    /// per frame.
    fn at(timeline: &Timeline, ordinal: u64) -> Self {
        let target = timeline.clamp_ordinal(ordinal);
        let mut reducer = TraceReducer::default();
        for idx in timeline.evicted()..target {
            if let Some(record) = timeline.get(idx) {
                reducer.apply_record(record);
            }
        }
        Self {
            cursor: target,
            reducer,
        }
    }

    /// Snap `cursor` back inside the ring's retained range. A long pause
    /// can let live eviction advance the horizon past `cursor`; the
    /// records in between are gone from the ring and unrecoverable, so
    /// the honest move is to drop to the horizon rather than fake a
    /// state that never happened.
    fn clamp_to_horizon(&mut self, timeline: &Timeline) {
        if self.cursor < timeline.evicted() {
            *self = Self::at(timeline, timeline.evicted());
        }
    }

    fn step_forward(&mut self, timeline: &Timeline) {
        self.clamp_to_horizon(timeline);
        if self.cursor >= timeline.end() {
            return;
        }
        if let Some(record) = timeline.get(self.cursor) {
            self.reducer.apply_record(record);
            self.cursor += 1;
        }
    }

    fn step_backward(&mut self, timeline: &Timeline) {
        self.clamp_to_horizon(timeline);
        if self.cursor > timeline.evicted() {
            *self = Self::at(timeline, self.cursor - 1);
        }
    }

    /// Jump by whole seconds from the last-folded record's `mono_us`,
    /// resolved to an ordinal through the ring's monotone `mono_us` index.
    fn jump_seconds(&mut self, timeline: &Timeline, delta_secs: i64) {
        self.clamp_to_horizon(timeline);
        let reference = self
            .cursor
            .checked_sub(1)
            .and_then(|idx| timeline.get(idx))
            .map(|record| record.mono_us)
            .or_else(|| timeline.oldest_mono_us())
            .unwrap_or(0);
        let delta_us = delta_secs.unsigned_abs() * 1_000_000;
        let target_mono = if delta_secs >= 0 {
            reference.saturating_add(delta_us)
        } else {
            reference.saturating_sub(delta_us)
        };
        let ordinal = timeline.ordinal_at_or_after(target_mono);
        *self = Self::at(timeline, ordinal);
    }
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
            daemon_epoch: None,
            addr,
            filter: ViewFilter::default(),
            focus: PaneFocus::default(),
            tree_cursor: None,
            view: AppView::default(),
            sandbox: SandboxMapState::default(),
            now_mono: 0,
            quit: false,
            dropped_events: 0,
            events_per_sec: 0.0,
            selected: None,
            collapsed: HashSet::new(),
            traces: TraceReducer::default(),
            event_times: VecDeque::new(),
            timeline: Timeline::new(),
            scrub: None,
        }
    }

    /// `true` while a scrub cursor is active, i.e. the view is frozen (or
    /// stepped) at some point in the past rather than tracking live.
    pub fn paused(&self) -> bool {
        self.scrub.is_some()
    }

    /// Single read accessor for render/selection code: the live reducer
    /// normally, or the scrub reducer while paused. Every accessor below
    /// that used to read `self.traces` forwards through this instead, so
    /// nothing has to remember to branch on `scrub` itself.
    fn view_reducer(&self) -> &TraceReducer {
        self.scrub
            .as_ref()
            .map_or(&self.traces, |scrub| &scrub.reducer)
    }

    /// Clock paired with [`Self::view_reducer`]: the live wall clock
    /// normally, or the `mono_us` of the last-folded record while
    /// scrubbed, so active-window pruning matches what the scrub reducer
    /// actually contains.
    pub fn view_now_mono(&self) -> u64 {
        let Some(scrub) = &self.scrub else {
            return self.now_mono;
        };
        scrub
            .cursor
            .checked_sub(1)
            .and_then(|idx| self.timeline.get(idx))
            .map_or(self.now_mono, |record| record.mono_us)
    }

    pub fn forest(&self) -> &MountForest {
        &self.view_reducer().forest
    }

    pub fn palette(&self) -> &MountPalette {
        &self.view_reducer().palette
    }

    pub fn selected_trace(&self) -> Option<TraceId> {
        self.selected
    }

    pub fn mount_window(&self, mount: &str) -> Option<&MountWindow> {
        self.view_reducer().mount_window(mount)
    }

    /// Sandbox port stats for one mount. Reads through [`Self::view_reducer`]
    /// like every other accessor, so time travel covers the sandbox map
    /// for free.
    pub fn mount_sandbox(&self, mount: &str) -> Option<MountSandboxView<'_>> {
        self.view_reducer().mount_sandbox(mount)
    }

    /// Mounts with any sandbox activity, most recent first.
    pub fn sandbox_mounts_by_activity(&self) -> Vec<&str> {
        self.view_reducer().sandbox_mounts_by_activity()
    }

    /// The mount the sandbox map renders this frame: `active_mount` if
    /// it has sandbox activity, otherwise the most recently active
    /// mount, otherwise `None`.
    pub fn sandbox_active_mount(&self) -> Option<&str> {
        self.view_reducer()
            .sandbox_active_mount(self.sandbox.active_mount.as_deref())
    }

    /// Number of records currently retained in the timeline ring: what
    /// scrubbing can actually reach, as opposed to `end()`'s raw
    /// arrival ordinal which also counts records already evicted.
    pub fn timeline_retained_count(&self) -> u64 {
        self.timeline.end().saturating_sub(self.timeline.evicted())
    }

    /// The oldest retained record's clock, if the timeline has anything
    /// at all.
    pub fn timeline_oldest_mono_us(&self) -> Option<u64> {
        self.timeline.oldest_mono_us()
    }

    pub fn ordered_mounts_for_strip(&self, cap: usize) -> Vec<String> {
        self.view_reducer().ordered_mounts_for_strip(cap)
    }

    pub fn operation(&self, trace_id: TraceId) -> Option<&Operation> {
        self.view_reducer().operation(trace_id)
    }

    pub fn visible_trace_ids(&self) -> Vec<TraceId> {
        self.view_reducer().visible_trace_ids(&self.filter)
    }

    /// Number of operations currently retained in memory. Pairs with
    /// [`MAX_RECENT_TRACES`] so subscribers can show eviction pressure.
    pub fn retained_trace_count(&self) -> usize {
        self.view_reducer().retained_trace_count()
    }

    pub const fn max_retained_traces() -> usize {
        MAX_RECENT_TRACES
    }

    /// Fold one record end-to-end: always into the live reducer and the
    /// timeline ring, regardless of pause. Pause freezes the *view*, not
    /// ingestion, so nothing is ever silently dropped while paused.
    pub fn apply_record(&mut self, record: InspectorRecord) {
        self.now_mono = record.mono_us;
        self.tick_event_window(record.mono_us);
        self.traces.apply_record(&record);

        // The live fold evicts the oldest retained trace once
        // MAX_RECENT_TRACES is exceeded; it no longer owns selection, so
        // a pointer to a just-evicted trace has to be caught and cleared
        // here. Checked against the current view (the scrub reducer
        // while paused) so a selection that's still visible there
        // survives unrelated live eviction during a long pause.
        if self
            .selected
            .is_some_and(|id| self.view_reducer().operation(id).is_none())
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

        self.timeline.push(record);
        self.ensure_selected_visible();
    }

    /// Reassign selection when it points at a trace that's no longer
    /// visible: evicted from the fold, or filtered out by the active
    /// [`ViewFilter`]. Falls back to the first currently visible trace.
    /// Runs against the view reducer, so pausing or stepping never resets
    /// a selection that's still valid at the current cursor.
    fn ensure_selected_visible(&mut self) {
        let selected_is_visible = self.selected.is_some_and(|id| {
            self.view_reducer()
                .operation(id)
                .is_some_and(|op| self.filter.matches(op))
        });
        if !selected_is_visible {
            self.selected = self
                .view_reducer()
                .visible_trace_ids(&self.filter)
                .first()
                .copied();
        }
    }

    pub fn apply_line(&mut self, line: InspectorLine) {
        match line {
            InspectorLine::Record(record) => self.apply_record(record),
            InspectorLine::Dropped { count } => {
                self.dropped_events = self.dropped_events.saturating_add(count);
            },
        }
    }

    fn begin_epoch(&mut self, epoch: String) {
        if self.daemon_epoch.as_deref() == Some(epoch.as_str()) {
            self.connected = true;
            return;
        }
        self.daemon_epoch = Some(epoch);
        self.connected = true;
        self.traces = TraceReducer::default();
        self.timeline = Timeline::new();
        self.event_times.clear();
        self.scrub = None;
        self.selected = None;
        self.tree_cursor = None;
        self.collapsed.clear();
        self.sandbox = SandboxMapState::default();
        self.now_mono = 0;
        self.events_per_sec = 0.0;
        self.dropped_events = 0;
    }

    /// Consume one source message: line payload or a connection-state
    /// transition. Pairs with [`super::source::EventSource::drain`].
    pub fn apply_source_message(&mut self, message: SourceMessage) {
        match message {
            SourceMessage::Line(line) => {
                self.apply_line(line);
            },
            SourceMessage::Connected { epoch } => self.begin_epoch(epoch),
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
            // Global keys: identical in both views.
            KeyCode::Char('v') => self.view = self.view.toggle(),
            KeyCode::Char(' ') => self.toggle_pause(),
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

            // Scrub controls: only meaningful while paused. While live
            // these keys are no-ops, matching the plain `_` arm below.
            KeyCode::Char('g') if self.scrub.is_some() => self.go_live(),
            KeyCode::Left
                if self.scrub.is_some() && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.jump_scrub_seconds(-1);
            },
            KeyCode::Right
                if self.scrub.is_some() && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.jump_scrub_seconds(1);
            },
            KeyCode::Left if self.scrub.is_some() => self.step_scrub_backward(),
            KeyCode::Right if self.scrub.is_some() => self.step_scrub_forward(),

            // Activity-only: tree/log focus, cursor movement, selection.
            KeyCode::Tab if self.view == AppView::Activity => self.focus = self.focus.cycle(),
            KeyCode::Up if self.view == AppView::Activity => self.move_focus_cursor(-1),
            KeyCode::Down if self.view == AppView::Activity => self.move_focus_cursor(1),
            KeyCode::Enter if self.view == AppView::Activity && self.focus == PaneFocus::Tree => {
                self.toggle_tree_cursor_collapse();
            },
            KeyCode::Char('j' | 'n') if self.view == AppView::Activity => self.select_next(),
            KeyCode::Char('k' | 'p') if self.view == AppView::Activity => self.select_prev(),

            // Sandbox-only: port cursor movement, pin, and mount cycling.
            KeyCode::Up if self.view == AppView::Sandbox => self.move_port_cursor(-1),
            KeyCode::Down if self.view == AppView::Sandbox => self.move_port_cursor(1),
            KeyCode::Enter if self.view == AppView::Sandbox => self.toggle_port_pin(),
            KeyCode::Char('m') if self.view == AppView::Sandbox => self.cycle_active_mount(),

            _ => {},
        }
    }

    /// Space: pause freezes the view at the current timeline position;
    /// pressing it again resumes live. Ingestion never stops either way.
    fn toggle_pause(&mut self) {
        if self.scrub.is_some() {
            self.go_live();
        } else {
            self.scrub = Some(Scrub::paused_at(&self.timeline));
            self.ensure_selected_visible();
        }
    }

    fn go_live(&mut self) {
        self.scrub = None;
        self.ensure_selected_visible();
    }

    fn step_scrub_forward(&mut self) {
        if let Some(scrub) = &mut self.scrub {
            scrub.step_forward(&self.timeline);
        }
        self.ensure_selected_visible();
    }

    fn step_scrub_backward(&mut self) {
        if let Some(scrub) = &mut self.scrub {
            scrub.step_backward(&self.timeline);
        }
        self.ensure_selected_visible();
    }

    fn jump_scrub_seconds(&mut self, delta_secs: i64) {
        if let Some(scrub) = &mut self.scrub {
            scrub.jump_seconds(&self.timeline, delta_secs);
        }
        self.ensure_selected_visible();
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
        let rows = self.forest().render_rows(
            self.view_now_mono(),
            ACTIVE_FOCUS_WINDOW_US,
            &self.collapsed,
        );
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

    /// `m`: advance `active_mount` to the next entry in
    /// [`Self::sandbox_mounts_by_activity`], wrapping from the last back
    /// to the first. Resolves the current position through
    /// [`Self::sandbox_active_mount`] so pressing `m` from an unset or
    /// stale selection lands on the mount right after whatever is
    /// currently displayed, matching what the user sees before the
    /// keypress.
    fn cycle_active_mount(&mut self) {
        let mounts = self.sandbox_mounts_by_activity();
        if mounts.is_empty() {
            self.sandbox.active_mount = None;
            return;
        }
        let current = self.sandbox_active_mount();
        let idx = current
            .and_then(|cur| mounts.iter().position(|m| *m == cur))
            .map_or(0, |i| (i + 1) % mounts.len());
        self.sandbox.active_mount = Some(mounts[idx].to_string());
    }

    /// ↑/↓ in the sandbox view: move the port cursor through the
    /// combined export-then-import list for the currently displayed
    /// mount, clamping at both ends (no wrap, unlike mount cycling).
    fn move_port_cursor(&mut self, delta: isize) {
        let mount = self.sandbox_active_mount();
        let sandbox = mount.and_then(|m| self.mount_sandbox(m));
        let ports = sandbox::all_port_ids(sandbox.as_ref());
        if ports.is_empty() {
            self.sandbox.selection = None;
            return;
        }
        let last = ports.len() - 1;
        let idx = match &self.sandbox.selection {
            Some(selection) => {
                let current = ports.iter().position(|p| *p == selection.port).unwrap_or(0);
                step_clamped(current, delta, last)
            },
            None if delta < 0 => last,
            None => 0,
        };
        let pinned = self.sandbox.selection.as_ref().is_some_and(|s| s.pinned);
        self.sandbox.selection = Some(PortSelection {
            port: ports[idx].clone(),
            pinned,
        });
    }

    fn toggle_port_pin(&mut self) {
        if let Some(selection) = &mut self.sandbox.selection {
            selection.pinned = !selection.pinned;
        }
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
        for id in self
            .view_reducer()
            .visible_trace_ids(&ViewFilter::default())
        {
            let Some(op) = self.view_reducer().operation(id) else {
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
        let visible = self.view_reducer().visible_trace_ids(&self.filter);
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
        let visible = self.view_reducer().visible_trace_ids(&self.filter);
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

#[cfg(test)]
mod tests {
    use omnifs_api::events::{OpEnd, OutcomeFields};

    use super::*;

    fn record(trace_id: TraceId, mono_us: u64, event: InspectorEvent) -> InspectorRecord {
        InspectorRecord::new("2026-05-23T12:00:00Z", mono_us, trace_id, event)
    }

    fn fuse_start(trace_id: TraceId, mono_us: u64, mount: &str, path: &str) -> InspectorRecord {
        record(
            trace_id,
            mono_us,
            InspectorEvent::FuseStart {
                op: "lookup".into(),
                mount: mount.into(),
                path: path.into(),
            },
        )
    }

    fn fuse_end(trace_id: TraceId, mono_us: u64, elapsed_us: u64) -> InspectorRecord {
        record(
            trace_id,
            mono_us,
            InspectorEvent::FuseEnd {
                op: "lookup".into(),
                end: OpEnd {
                    elapsed_us,
                    result: OutcomeFields::ok(),
                },
            },
        )
    }

    /// A `provider.start` record: the minimal event that gives a mount
    /// sandbox activity (`ProviderStart` carries its own `mount`, so
    /// unlike callouts it doesn't need a matching `fuse.start` first).
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

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn pause_is_lossless_new_records_land_live_but_not_in_the_frozen_view() {
        let mut app = App::new(ConnectionMode::Replay, "test", None);
        app.apply_record(fuse_start(1, 10, "github", "/a"));
        app.apply_record(fuse_end(1, 20, 10));

        app.toggle_pause();
        assert!(app.paused());

        // Records keep arriving while paused; ingestion never drops them.
        app.apply_record(fuse_start(2, 30, "github", "/b"));
        app.apply_record(fuse_end(2, 40, 10));
        assert_eq!(app.timeline.end(), 4);

        // The frozen view doesn't show them yet...
        assert!(app.operation(2).is_none());
        // ...but resuming live surfaces the full history, including what
        // arrived during the pause.
        app.go_live();
        assert!(!app.paused());
        assert!(app.operation(2).is_some());
    }

    #[test]
    fn inspector_epoch_reset_drops_old_timeline_but_same_epoch_reconnect_keeps_it() {
        let mut app = App::new(ConnectionMode::Inspector, "test", Some("daemon".into()));
        app.apply_source_message(SourceMessage::Connected {
            epoch: "one".into(),
        });
        app.apply_source_message(SourceMessage::Line(InspectorLine::Record(
            fuse_start(1, 100, "github", "/a").with_seq(1),
        )));
        app.apply_source_message(SourceMessage::Connected {
            epoch: "one".into(),
        });
        assert_eq!(app.timeline_retained_count(), 1);

        app.apply_source_message(SourceMessage::Connected {
            epoch: "two".into(),
        });
        assert_eq!(app.timeline_retained_count(), 0);
        assert_eq!(app.now_mono, 0);
        app.apply_source_message(SourceMessage::Line(InspectorLine::Record(
            fuse_start(2, 3, "github", "/b").with_seq(1),
        )));
        assert_eq!(app.timeline_oldest_mono_us(), Some(3));
        assert_eq!(app.selected_trace(), Some(2));
    }

    #[test]
    fn scrub_stepping_reflects_prefix_state() {
        let mut app = App::new(ConnectionMode::Replay, "test", None);
        app.apply_record(fuse_start(1, 10, "github", "/a"));
        app.apply_record(fuse_end(1, 20, 10));
        app.apply_record(fuse_start(2, 30, "github", "/b"));
        app.apply_record(fuse_end(2, 40, 10));

        app.toggle_pause();
        assert!(app.operation(2).is_some());

        // Undo trace 2's fuse.end: it's back to running.
        app.step_scrub_backward();
        assert_eq!(
            app.operation(2)
                .expect("trace 2 started in this prefix")
                .status,
            crate::inspector::trace_state::OperationStatus::Running
        );

        // Undo trace 2's fuse.start: it hasn't happened yet in this prefix.
        app.step_scrub_backward();
        assert!(app.operation(2).is_none());

        // Step forward again: trace 2 reappears.
        app.step_scrub_forward();
        assert!(app.operation(2).is_some());
    }

    #[test]
    fn horizon_clamp_does_not_panic_when_the_cursor_falls_behind_the_ring() {
        let mut timeline = Timeline::with_capacity(2);
        timeline.push(fuse_start(1, 10, "github", "/a"));
        timeline.push(fuse_end(1, 20, 10));

        let mut scrub = Scrub::paused_at(&timeline);
        scrub.step_backward(&timeline);
        scrub.step_backward(&timeline);
        assert_eq!(scrub.cursor, 0);

        // Push past capacity: the ring evicts every record the cursor
        // referenced.
        timeline.push(fuse_start(2, 30, "github", "/b"));
        timeline.push(fuse_end(2, 40, 10));
        timeline.push(fuse_start(3, 50, "github", "/c"));
        assert_eq!(timeline.evicted(), 3);

        // Stepping must clamp to the horizon instead of indexing an
        // evicted ordinal.
        scrub.step_forward(&timeline);
        assert_eq!(scrub.cursor, 4);
        assert!(scrub.reducer.operation(3).is_none());
    }

    #[test]
    fn v_toggles_between_activity_and_sandbox_views() {
        let mut app = App::new(ConnectionMode::Replay, "test", None);
        assert_eq!(app.view, AppView::Activity);
        app.handle_key(key(crossterm::event::KeyCode::Char('v')));
        assert_eq!(app.view, AppView::Sandbox);
        app.handle_key(key(crossterm::event::KeyCode::Char('v')));
        assert_eq!(app.view, AppView::Activity);
    }

    #[test]
    fn port_cursor_moves_through_exports_then_imports_and_clamps_at_both_ends() {
        use crossterm::event::KeyCode;

        let mut app = App::new(ConnectionMode::Replay, "test", None);
        app.apply_record(provider_start(1, 10, "github", "lookup_child"));
        app.view = AppView::Sandbox;

        app.handle_key(key(KeyCode::Down));
        assert_eq!(
            app.sandbox
                .selection
                .as_ref()
                .map(|selection| &selection.port),
            Some(&PortId::Export(sandbox::EXPORT_PORTS[0].to_string()))
        );

        // Up from the first row must clamp, not go negative or wrap.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(
            app.sandbox
                .selection
                .as_ref()
                .map(|selection| &selection.port),
            Some(&PortId::Export(sandbox::EXPORT_PORTS[0].to_string()))
        );

        // Walking past the end of the combined list must clamp on Log,
        // the last row, rather than panicking or wrapping.
        let total = sandbox::all_port_ids(app.mount_sandbox("github").as_ref()).len();
        for _ in 0..total + 3 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(
            app.sandbox
                .selection
                .as_ref()
                .map(|selection| &selection.port),
            Some(&PortId::Log)
        );
    }

    #[test]
    fn m_cycles_active_mount_through_sandbox_activity_order() {
        use crossterm::event::KeyCode;

        let mut app = App::new(ConnectionMode::Replay, "test", None);
        app.apply_record(provider_start(1, 10, "github", "lookup_child"));
        app.apply_record(provider_start(2, 20, "gitlab", "lookup_child"));
        app.view = AppView::Sandbox;

        // Most-recently-active mount first, with nothing pinned yet.
        assert_eq!(app.sandbox_active_mount(), Some("gitlab"));

        app.handle_key(key(KeyCode::Char('m')));
        assert_eq!(app.sandbox.active_mount.as_deref(), Some("github"));

        app.handle_key(key(KeyCode::Char('m')));
        assert_eq!(app.sandbox.active_mount.as_deref(), Some("gitlab"));
    }
}
