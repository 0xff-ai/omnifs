//! TUI state: operation store, mount windows, filters.

use std::collections::{HashSet, VecDeque};

use crossterm::event::{KeyCode, KeyEvent};
use omnifs_api::events::{InspectorEvent, InspectorLine, InspectorRecord, TraceId};

use super::filter::ViewFilter;
use super::metrics::LatencyWindow;
use super::sandbox::{MountSandboxView, PortId};
use super::source::{ReplaySpeed, SourceMessage};
use super::timeline::Timeline;
use super::trace_state::{
    MAX_RECENT_TRACES, MountPalette, Operation, OperationStatus, SessionStats, TraceReducer,
};
use super::tree::{ACTIVE_FOCUS_WINDOW_US, MountForest, RenderRow};

const EVENT_WINDOW: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OperationOrder {
    #[default]
    Recent,
    Latency,
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
    pub(crate) fn toggle(self) -> Self {
        match self {
            Self::Activity => Self::Sandbox,
            Self::Sandbox => Self::Activity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SandboxMapState {
    pub active_mount: Option<String>,
    pub selection: Option<PortId>,
}

impl SandboxMapState {
    /// Advance `active_mount` to the next entry in `ordered`, wrapping from
    /// the last back to the first. `current` is the mount the sandbox map
    /// is effectively displaying right now; resolving that from an unset or
    /// stale `active_mount` needs the trace reducer's activity view, so the
    /// caller computes it and passes it in rather than this method reading
    /// `self.active_mount` directly.
    pub(crate) fn cycle_active_mount(&mut self, current: Option<&str>, ordered: &[&str]) {
        if ordered.is_empty() {
            self.active_mount = None;
            return;
        }
        let idx = current
            .and_then(|cur| ordered.iter().position(|mount| *mount == cur))
            .map_or(0, |i| (i + 1) % ordered.len());
        self.active_mount = Some(ordered[idx].to_string());
    }

    /// Move `selection` through `ports` by `delta`, clamping at both ends
    /// (no wrap, unlike mount cycling).
    pub(crate) fn move_selection(&mut self, delta: isize, ports: &[PortId]) {
        if ports.is_empty() {
            self.selection = None;
            return;
        }
        let last = ports.len() - 1;
        let idx = match &self.selection {
            Some(selection) => {
                let current = ports.iter().position(|p| p == selection).unwrap_or(0);
                step_clamped(current, delta, last)
            },
            None if delta < 0 => last,
            None => 0,
        };
        self.selection = Some(ports[idx].clone());
    }
}

// This is a UI-state struct whose bools are independent toggles
// (connection observed by the source thread and quit signal), not an
// implicit state machine crying out for an enum.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    /// Whether this session replays a finite captured file rather than
    /// attaching to a live daemon. Mirrors the [`super::source::SourceKind`]
    /// the caller constructed the session from.
    pub is_replay: bool,
    pub container: String,
    /// True only after the source thread reports at least one
    /// successful TCP connect. Stays false through every silent
    /// reconnect attempt so the header never lies about reachability.
    pub connected: bool,
    daemon_epoch: Option<String>,
    /// Inspector address shown in the header while disconnected.
    /// `None` while `is_replay`.
    pub addr: Option<String>,
    /// Hides mounts with no samples in the current metrics window.
    pub hide_idle: bool,
    pub operation_order: OperationOrder,
    pub help_open: bool,
    pub replay_speed: ReplaySpeed,
    pub notice: Option<String>,
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
    /// A usable path shown in the activity view's empty state.
    pub teaching_path: Option<String>,
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
    yank_request: Option<String>,
}

/// A paused view: a `TraceReducer` folded up to `cursor`, an absolute
/// timeline ordinal one past the last folded record. Rebuilt by
/// refolding a prefix of the ring rather than trying to run the fold
/// backward, since `TraceReducer::apply_record` has no inverse.
struct Scrub {
    cursor: u64,
    live_at_pause: u64,
    reducer: TraceReducer,
}

impl Scrub {
    /// Fold the entire ring: the state a fresh pause freezes on.
    fn paused_at(timeline: &Timeline) -> Self {
        let end = timeline.end();
        Self::at(timeline, end, end)
    }

    /// Rebuild from scratch by folding `ring[evicted..ordinal)`, clamped
    /// into the retained range. O(ring length), but this only runs on
    /// pause, backward steps, and second-granularity jumps, never once
    /// per frame.
    fn at(timeline: &Timeline, ordinal: u64, live_at_pause: u64) -> Self {
        let target = timeline.clamp_ordinal(ordinal);
        let mut reducer = TraceReducer::default();
        for idx in timeline.evicted()..target {
            if let Some(record) = timeline.get(idx) {
                reducer.apply_record(record);
            }
        }
        Self {
            cursor: target,
            live_at_pause,
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
            *self = Self::at(timeline, timeline.evicted(), self.live_at_pause);
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
            *self = Self::at(timeline, self.cursor - 1, self.live_at_pause);
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
        *self = Self::at(timeline, ordinal, self.live_at_pause);
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
    pub(crate) fn cycle(self) -> Self {
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
    fn locate_or_nearest_ancestor(&self, rows: &[RenderRow]) -> usize {
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
    pub fn new(
        is_replay: bool,
        container: impl Into<String>,
        addr: Option<String>,
        teaching_path: Option<String>,
    ) -> Self {
        Self {
            is_replay,
            container: container.into(),
            // Start false for a live attach: connection state is only
            // honest once the source thread actually attaches. Replay
            // sessions ignore this flag in the header.
            connected: false,
            daemon_epoch: None,
            addr,
            hide_idle: false,
            operation_order: OperationOrder::Recent,
            help_open: false,
            replay_speed: ReplaySpeed::Normal,
            notice: None,
            filter: ViewFilter::default(),
            focus: PaneFocus::default(),
            tree_cursor: None,
            view: AppView::default(),
            sandbox: SandboxMapState::default(),
            now_mono: 0,
            quit: false,
            dropped_events: 0,
            events_per_sec: 0.0,
            teaching_path,
            selected: None,
            collapsed: HashSet::new(),
            traces: TraceReducer::default(),
            event_times: VecDeque::new(),
            timeline: Timeline::new(),
            scrub: None,
            yank_request: None,
        }
    }

    /// `true` while a scrub cursor is active, i.e. the view is frozen (or
    /// stepped) at some point in the past rather than tracking live.
    pub fn paused(&self) -> bool {
        self.scrub.is_some()
    }

    /// Records received after the current pause began. Moving the scrub
    /// cursor does not change this count.
    pub fn buffered_since_pause(&self) -> u64 {
        self.scrub.as_ref().map_or(0, |scrub| {
            self.timeline.end().saturating_sub(scrub.live_at_pause)
        })
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

    pub fn mount_window(&self, mount: &str) -> Option<&LatencyWindow> {
        self.view_reducer().mount_window(mount)
    }

    /// True when a mount has no samples in its current metrics window.
    pub fn mount_is_idle(&self, mount: &str) -> bool {
        self.mount_window(mount).is_none_or(LatencyWindow::is_empty)
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

    pub fn ordered_mounts_for_strip(&self, cap: usize) -> Vec<String> {
        let mounts = self.view_reducer().ordered_mounts_for_strip(cap);
        if self.hide_idle {
            mounts
                .into_iter()
                .filter(|mount| !self.mount_is_idle(mount))
                .collect()
        } else {
            mounts
        }
    }

    /// Tree rows shared by rendering and navigation, including external
    /// collapse state and the idle-mount filter.
    pub fn visible_tree_rows(&self) -> Vec<RenderRow> {
        let rows = self.forest().render_rows(
            self.view_now_mono(),
            ACTIVE_FOCUS_WINDOW_US,
            &self.collapsed,
        );
        if self.hide_idle {
            rows.into_iter()
                .filter(|row| !self.mount_is_idle(&row.mount))
                .collect()
        } else {
            rows
        }
    }

    pub fn operation(&self, trace_id: TraceId) -> Option<&Operation> {
        self.view_reducer().operation(trace_id)
    }

    pub fn visible_trace_ids(&self) -> Vec<TraceId> {
        let mut traces = self.view_reducer().visible_trace_ids(&self.filter);
        if self.operation_order == OperationOrder::Latency {
            traces.sort_by_key(|trace_id| {
                std::cmp::Reverse(
                    self.operation(*trace_id)
                        .and_then(|operation| operation.fuse_elapsed_us)
                        .unwrap_or(0),
                )
            });
        }
        traces
    }

    /// Number of operations currently retained in memory. Pairs with
    /// [`MAX_RECENT_TRACES`] so subscribers can show eviction pressure.
    pub fn retained_trace_count(&self) -> usize {
        self.view_reducer().retained_trace_count()
    }

    pub const fn max_retained_traces() -> usize {
        MAX_RECENT_TRACES
    }

    /// Durable whole-session counters from the live reducer. Scrubbing
    /// affects the view, never the quit receipt.
    pub fn session(&self) -> &SessionStats {
        self.traces.session()
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
            self.selected = self.visible_trace_ids().first().copied();
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
    /// transition. Pairs with [`super::source::EventSource::drain_frame`].
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

    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.filter.editing {
            self.handle_filter_key(key.code);
            return;
        }
        if self.help_open {
            if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
                self.help_open = false;
            } else if super::keymap::quit_requested(&key) {
                self.quit = true;
            }
            return;
        }
        super::keymap::dispatch(self, &key);
    }

    pub fn take_yank_request(&mut self) -> Option<String> {
        self.yank_request.take()
    }

    /// Space: pause freezes the view at the current timeline position;
    /// pressing it again resumes live. Ingestion never stops either way.
    pub(crate) fn toggle_pause(&mut self) {
        if self.scrub.is_some() {
            self.go_live();
        } else {
            self.scrub = Some(Scrub::paused_at(&self.timeline));
            self.ensure_selected_visible();
        }
    }

    pub(crate) fn go_live(&mut self) {
        self.scrub = None;
        self.ensure_selected_visible();
    }

    pub(crate) fn step_scrub_forward(&mut self) {
        if let Some(scrub) = &mut self.scrub {
            scrub.step_forward(&self.timeline);
        }
        self.ensure_selected_visible();
    }

    pub(crate) fn step_scrub_backward(&mut self) {
        if let Some(scrub) = &mut self.scrub {
            scrub.step_backward(&self.timeline);
        }
        self.ensure_selected_visible();
    }

    pub(crate) fn jump_scrub_seconds(&mut self, delta_secs: i64) {
        if let Some(scrub) = &mut self.scrub {
            scrub.jump_seconds(&self.timeline, delta_secs);
        }
        self.ensure_selected_visible();
    }

    pub(crate) fn move_focus_cursor(&mut self, delta: isize) {
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
        let rows = self.visible_tree_rows();
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

    /// `m`: advance the sandbox map's active mount, wrapping from the last
    /// back to the first. Gathers the domain inputs
    /// [`SandboxMapState::cycle_active_mount`] needs (the activity-ordered
    /// mount list, and the mount currently displayed, resolved through
    /// [`Self::sandbox_active_mount`] so pressing `m` from an unset or stale
    /// selection lands on the mount right after whatever the user is
    /// currently looking at) and delegates the index math to it.
    pub(crate) fn cycle_active_mount(&mut self) {
        let ordered = self.sandbox_mounts_by_activity();
        let current = self.sandbox_active_mount();
        // Own both before mutating `self.sandbox`: they otherwise borrow
        // the trace reducer through `&self`, which would still be live for
        // the duration of the call below.
        let current = current.map(str::to_string);
        let ordered: Vec<String> = ordered.into_iter().map(str::to_string).collect();
        let ordered: Vec<&str> = ordered.iter().map(String::as_str).collect();
        self.sandbox
            .cycle_active_mount(current.as_deref(), &ordered);
    }

    /// ↑/↓ in the sandbox view: move the port cursor through the combined
    /// export-then-import list for the currently displayed mount.
    pub(crate) fn move_port_cursor(&mut self, delta: isize) {
        let ports = PortId::all();
        self.sandbox.move_selection(delta, &ports);
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

    pub(crate) fn toggle_tree_cursor_collapse(&mut self) {
        let Some(cursor) = self.tree_cursor.clone() else {
            return;
        };
        let key = (cursor.mount, cursor.path);
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
    }

    fn handle_filter_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.filter.cancel_edit(),
            KeyCode::Enter => self.filter.commit_edit(),
            KeyCode::Backspace => self.filter.backspace(),
            KeyCode::Char(ch) => self.filter.push_char(ch),
            _ => {},
        }
    }

    /// Slides the fixed-size recent-event window and recomputes
    /// `events_per_sec` from it via the shared
    /// [`super::metrics::rate_per_sec`], the same rate arithmetic
    /// `LatencyWindow::event_rate_per_sec` uses for its own time-bounded
    /// window. This window stays a fixed recent-event count rather than
    /// switching to `LatencyWindow`'s time-bounded policy: the header rate
    /// must stay live from the last handful of events even when traffic is
    /// sparse, where a 60s time window would read 0 between bursts.
    fn tick_event_window(&mut self, mono_us: u64) {
        self.event_times.push_back(mono_us);
        while self.event_times.len() > EVENT_WINDOW {
            self.event_times.pop_front();
        }
        let oldest = self.event_times.front().copied().unwrap_or(mono_us);
        self.events_per_sec = super::metrics::rate_per_sec(self.event_times.len(), oldest, mono_us);
    }

    pub(crate) fn reset_recent(&mut self) {
        self.traces.reset_recent();
        if let Some(scrub) = &mut self.scrub {
            scrub.reducer.reset_recent();
        }
        self.ensure_selected_visible();
    }

    pub(crate) fn select_next(&mut self) {
        let visible = self.visible_trace_ids();
        if visible.is_empty() {
            return;
        }
        let idx = self
            .selected
            .and_then(|sel| visible.iter().position(|id| *id == sel))
            .map_or(0, |i| (i + 1).min(visible.len() - 1));
        self.selected = Some(visible[idx]);
        self.sync_tree_cursor_to_selection();
    }

    pub(crate) fn select_prev(&mut self) {
        let visible = self.visible_trace_ids();
        if visible.is_empty() {
            return;
        }
        let idx = self
            .selected
            .and_then(|sel| visible.iter().position(|id| *id == sel))
            .map_or(0, |i| i.saturating_sub(1));
        self.selected = Some(visible[idx]);
        self.sync_tree_cursor_to_selection();
    }

    pub(crate) fn select_next_error(&mut self) {
        let visible = self.visible_trace_ids();
        let errors: Vec<_> = visible
            .into_iter()
            .filter(|trace_id| {
                self.operation(*trace_id)
                    .is_some_and(|operation| operation.status == OperationStatus::Error)
            })
            .collect();
        if errors.is_empty() {
            self.notice = Some("No failed operations in this view.".into());
            return;
        }
        let next = self
            .selected
            .and_then(|selected| errors.iter().position(|trace_id| *trace_id == selected))
            .map_or(errors[0], |index| errors[(index + 1) % errors.len()]);
        self.selected = Some(next);
        self.sync_tree_cursor_to_selection();
    }

    fn sync_tree_cursor_to_selection(&mut self) {
        let Some((mount, path)) = self
            .selected
            .and_then(|trace_id| self.operation(trace_id))
            .map(|operation| (operation.mount.clone(), operation.path.clone()))
        else {
            return;
        };
        self.tree_cursor = Some(TreeCursor { mount, path });
    }

    pub(crate) fn toggle_errors_only(&mut self) {
        self.filter.toggle_errors_only();
        self.ensure_selected_visible();
    }

    pub(crate) fn toggle_idle(&mut self) {
        self.hide_idle = !self.hide_idle;
    }

    pub(crate) fn toggle_operation_order(&mut self) {
        self.operation_order = match self.operation_order {
            OperationOrder::Recent => OperationOrder::Latency,
            OperationOrder::Latency => OperationOrder::Recent,
        };
        self.ensure_selected_visible();
    }

    pub(crate) fn request_yank(&mut self) {
        let Some(path) = self
            .selected
            .and_then(|trace_id| self.operation(trace_id))
            .map(|operation| operation.path.clone())
        else {
            self.notice = Some("No operation path is selected.".into());
            return;
        };
        self.yank_request = Some(path);
    }

    pub(crate) fn start_filter_edit(&mut self) {
        self.filter.begin_edit();
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
        let mut app = App::new(true, "test", None, Some("/omnifs".into()));
        app.apply_record(fuse_start(1, 10, "github", "/a"));
        app.apply_record(fuse_end(1, 20, 10));

        app.toggle_pause();
        assert!(app.paused());

        // Records keep arriving while paused; ingestion never drops them.
        app.apply_record(fuse_start(2, 30, "github", "/b"));
        app.apply_record(fuse_end(2, 40, 10));
        assert_eq!(app.timeline.end(), 4);
        assert_eq!(app.buffered_since_pause(), 2);

        // The frozen view doesn't show them yet...
        assert!(app.operation(2).is_none());
        // ...but resuming live surfaces the full history, including what
        // arrived during the pause.
        app.go_live();
        assert!(!app.paused());
        assert!(app.operation(2).is_some());
        assert_eq!(app.buffered_since_pause(), 0);
    }

    #[test]
    fn inspector_epoch_reset_drops_old_timeline_but_same_epoch_reconnect_keeps_it() {
        let mut app = App::new(false, "test", Some("daemon".into()), Some("/omnifs".into()));
        app.apply_source_message(SourceMessage::Connected {
            epoch: "one".into(),
        });
        app.apply_source_message(SourceMessage::Line(InspectorLine::Record(
            fuse_start(1, 100, "github", "/a").with_seq(1),
        )));
        app.apply_source_message(SourceMessage::Connected {
            epoch: "one".into(),
        });
        assert_eq!(app.timeline.end() - app.timeline.evicted(), 1);

        app.apply_source_message(SourceMessage::Connected {
            epoch: "two".into(),
        });
        assert_eq!(app.timeline.end() - app.timeline.evicted(), 0);
        assert_eq!(app.now_mono, 0);
        app.apply_source_message(SourceMessage::Line(InspectorLine::Record(
            fuse_start(2, 3, "github", "/b").with_seq(1),
        )));
        assert_eq!(app.timeline.oldest_mono_us(), Some(3));
        assert_eq!(app.selected_trace(), Some(2));
    }

    #[test]
    fn scrub_stepping_reflects_prefix_state() {
        let mut app = App::new(true, "test", None, Some("/omnifs".into()));
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
            crate::trace_state::OperationStatus::Running
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
        let mut app = App::new(true, "test", None, Some("/omnifs".into()));
        assert_eq!(app.view, AppView::Activity);
        app.handle_key(key(crossterm::event::KeyCode::Char('v')));
        assert_eq!(app.view, AppView::Sandbox);
        app.handle_key(key(crossterm::event::KeyCode::Char('v')));
        assert_eq!(app.view, AppView::Activity);
    }

    #[test]
    fn port_cursor_moves_through_exports_then_imports_and_clamps_at_both_ends() {
        use crossterm::event::KeyCode;

        let mut app = App::new(true, "test", None, Some("/omnifs".into()));
        app.apply_record(provider_start(1, 10, "github", "lookup_child"));
        app.view = AppView::Sandbox;

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.sandbox.selection.as_ref(), PortId::exports().first());

        // Up from the first row must clamp, not go negative or wrap.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.sandbox.selection.as_ref(), PortId::exports().first());

        // Walking past the end of the combined list must clamp on the
        // last static row, rather than panicking or wrapping.
        let total = PortId::all().len();
        for _ in 0..total + 3 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(
            app.sandbox.selection.as_ref(),
            Some(&PortId::Import(
                omnifs_api::events::CalloutKind::GitOpenRepo
            ))
        );
    }

    #[test]
    fn m_cycles_active_mount_through_sandbox_activity_order() {
        use crossterm::event::KeyCode;

        let mut app = App::new(true, "test", None, Some("/omnifs".into()));
        app.apply_record(provider_start(1, 10, "github", "lookup_child"));
        app.apply_record(provider_start(2, 20, "gitlab", "lookup_child"));
        app.view = AppView::Sandbox;

        // Most-recently-active mount first, with no explicit mount yet.
        assert_eq!(app.sandbox_active_mount(), Some("gitlab"));

        app.handle_key(key(KeyCode::Char('m')));
        assert_eq!(app.sandbox.active_mount.as_deref(), Some("github"));

        app.handle_key(key(KeyCode::Char('m')));
        assert_eq!(app.sandbox.active_mount.as_deref(), Some("gitlab"));
    }

    #[test]
    fn pause_time_selection_and_collapse_survive_resume() {
        let mut app = App::new(true, "test", None, Some("/omnifs".into()));
        app.apply_record(fuse_start(1, 10, "github", "/dir/one"));
        app.apply_record(fuse_start(2, 20, "github", "/dir/two"));
        assert_eq!(app.selected_trace(), Some(1));

        app.toggle_pause();
        app.select_prev();
        assert_eq!(app.selected_trace(), Some(2));
        app.move_tree_cursor(1);
        app.toggle_tree_cursor_collapse();
        assert!(app.visible_tree_rows().iter().any(|row| {
            row.path
                .ends_with(super::super::tree::COLLAPSED_SUMMARY_SUFFIX)
        }));

        app.go_live();
        assert_eq!(app.selected_trace(), Some(2));
        assert!(app.visible_tree_rows().iter().any(|row| {
            row.path
                .ends_with(super::super::tree::COLLAPSED_SUMMARY_SUFFIX)
        }));
    }

    #[test]
    fn idle_toggle_hides_and_restores_mounts() {
        let mut app = App::new(true, "test", None, Some("/omnifs".into()));
        app.apply_record(fuse_start(1, 10, "github", "/a"));
        assert!(app.mount_is_idle("github"));
        assert_eq!(app.ordered_mounts_for_strip(8), vec!["github"]);

        app.handle_key(key(crossterm::event::KeyCode::Char('i')));
        assert!(app.ordered_mounts_for_strip(8).is_empty());
        assert!(app.visible_tree_rows().is_empty());

        app.handle_key(key(crossterm::event::KeyCode::Char('i')));
        assert_eq!(app.ordered_mounts_for_strip(8), vec!["github"]);
        assert!(!app.visible_tree_rows().is_empty());
    }

    #[test]
    fn operation_controls_share_the_keymap_and_keep_selection_in_sync() {
        let mut app = App::new(true, "test", None, Some("/omnifs".into()));
        app.apply_record(fuse_start(1, 10, "github", "/fast"));
        app.apply_record(fuse_end(1, 20, 10));
        app.apply_record(fuse_start(2, 30, "github", "/slow"));
        app.apply_record(fuse_end(2, 40, 200_000));
        app.apply_record(fuse_start(3, 50, "github", "/failed"));
        app.apply_record(record(
            3,
            60,
            InspectorEvent::FuseEnd {
                op: "lookup".into(),
                end: OpEnd {
                    elapsed_us: 30,
                    result: OutcomeFields::with_outcome(
                        omnifs_api::events::InspectorOutcome::Network,
                    ),
                },
            },
        ));

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.operation_order, OperationOrder::Latency);
        assert_eq!(app.visible_trace_ids()[0], 2);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected_trace(), Some(3));
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected_trace(), Some(2));

        app.handle_key(key(KeyCode::Char('E')));
        assert_eq!(app.selected_trace(), Some(3));
        assert_eq!(
            app.tree_cursor.as_ref().map(|cursor| cursor.path.as_str()),
            Some("/failed")
        );

        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.take_yank_request().as_deref(), Some("/failed"));

        app.handle_key(key(KeyCode::Char(']')));
        assert_eq!(app.replay_speed, ReplaySpeed::Double);

        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.help_open);
        let help = crate::keymap::help_lines(&app).join("\n");
        assert!(help.contains("E        next error"));
        assert!(help.contains("]        faster"));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.help_open);
        assert!(!app.quit);
    }
}
