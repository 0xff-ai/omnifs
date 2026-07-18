//! TUI state: operation store, mount windows, filters.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use omnifs_api::events::{InspectorLine, InspectorRecord, TraceId};

use super::filter::{FilterMode, ViewFilter};
use super::metrics::MountWindow;
use super::source::SourceMessage;
use super::trace_state::{MAX_RECENT_TRACES, MountPalette, Operation, SessionStats, TraceReducer};
use super::tree::{ACTIVE_FOCUS_WINDOW_US, MountForest, RenderRow};

const EVENT_WINDOW: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionMode {
    Inspector,
    Replay,
}

// Every bool here is an independent, orthogonal toggle (connection status,
// view pause, idle-mount filter, quit signal) rather than encoded state
// machine phases, so collapsing them into an enum would just reintroduce
// the same four-way cross product under a different name.
#[allow(clippy::struct_excessive_bools)]
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
    /// Hides mounts (and their tree rows) with no activity in the current
    /// metrics window. Toggled by the `i` binding.
    pub hide_idle: bool,
    pub filter: ViewFilter,
    pub focus: PaneFocus,
    pub tree_cursor: Option<TreeCursor>,
    pub now_mono: u64,
    pub quit: bool,
    pub dropped_events: u64,
    pub events_per_sec: f64,
    /// A real-ish path to teach in the empty state, e.g. `cat <this>/...`.
    /// Resolved once at startup from local mount specs (see
    /// `commands/inspect.rs`); never a daemon round trip.
    pub teaching_path: String,
    /// The live, ever-advancing reducer. `apply_record` always writes here,
    /// paused or not, so pausing the view can never drop data.
    traces: TraceReducer,
    /// Snapshot of `traces` taken the moment the user paused, plus the count
    /// of records folded into `traces` since. Rendering and read-only
    /// navigation go through [`App::active`]/[`App::view_now_mono`], which
    /// prefer this snapshot while it's set; resuming drops it so the view
    /// jumps straight to the caught-up live state.
    frozen: Option<FrozenView>,
    event_times: VecDeque<u64>,
}

struct FrozenView {
    traces: TraceReducer,
    now_mono: u64,
    buffered: u64,
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
        mode: ConnectionMode,
        container: impl Into<String>,
        addr: Option<String>,
        teaching_path: impl Into<String>,
    ) -> Self {
        Self {
            mode,
            container: container.into(),
            // Start false in Inspector mode: connection state is only
            // honest once the source thread actually attaches. Replay
            // mode ignores this flag in the header.
            connected: false,
            addr,
            paused: false,
            hide_idle: false,
            filter: ViewFilter::default(),
            focus: PaneFocus::default(),
            tree_cursor: None,
            now_mono: 0,
            quit: false,
            dropped_events: 0,
            events_per_sec: 0.0,
            teaching_path: teaching_path.into(),
            traces: TraceReducer::default(),
            frozen: None,
            event_times: VecDeque::new(),
        }
    }

    /// The reducer state to render and navigate: the live reducer normally,
    /// or the snapshot frozen at pause time while `paused`.
    fn active(&self) -> &TraceReducer {
        self.frozen.as_ref().map_or(&self.traces, |f| &f.traces)
    }

    /// Mutable counterpart of [`App::active`], for user-driven navigation
    /// (selection, collapse) that should feel live even while paused. Takes
    /// the two fields directly rather than being a `&mut self` method so
    /// callers can still borrow `self.filter` in the same expression.
    fn active_mut<'a>(
        traces: &'a mut TraceReducer,
        frozen: &'a mut Option<FrozenView>,
    ) -> &'a mut TraceReducer {
        frozen.as_mut().map_or(traces, |f| &mut f.traces)
    }

    /// The `now_mono` to render against: frozen at pause time so active-focus
    /// windows and sparklines don't visibly age while the screen is frozen.
    pub fn view_now_mono(&self) -> u64 {
        self.frozen.as_ref().map_or(self.now_mono, |f| f.now_mono)
    }

    /// Records folded into the live reducer since the user paused. Zero
    /// while not paused.
    pub fn buffered_since_pause(&self) -> u64 {
        self.frozen.as_ref().map_or(0, |f| f.buffered)
    }

    fn toggle_pause(&mut self) {
        if self.paused {
            // Resume: drop the snapshot so the very next frame renders
            // everything that accrued while paused.
            self.paused = false;
            self.frozen = None;
        } else {
            self.paused = true;
            self.frozen = Some(FrozenView {
                traces: self.traces.clone(),
                now_mono: self.now_mono,
                buffered: 0,
            });
        }
    }

    pub fn forest(&self) -> &MountForest {
        &self.active().forest
    }

    /// Project whatever the active view (frozen while paused, live
    /// otherwise) just settled on as selected onto the live reducer too.
    /// Without this, a selection change made while paused only lands on
    /// the frozen snapshot, and resuming (which drops the snapshot)
    /// silently reverts it back to whatever the live reducer picked on
    /// its own while catching up in the background. The live reducer's
    /// own `ensure_selected_visible` calls (in `apply_record`) then keep
    /// covering the case where the projected trace later gets evicted.
    fn sync_selected_to_live(&mut self) {
        let target = self.active().selected();
        self.traces.set_selected(target);
    }

    pub fn palette(&self) -> &MountPalette {
        &self.active().palette
    }

    pub fn selected_trace(&self) -> Option<TraceId> {
        self.active().selected()
    }

    pub fn mount_window(&self, mount: &str) -> Option<&MountWindow> {
        self.active().mount_window(mount)
    }

    /// True when `mount` has no samples in its current metrics window (or no
    /// window at all yet). Drives the `i` idle-hide toggle; matches the
    /// sparkline strip's own "idle" label so the two never disagree.
    pub fn mount_is_idle(&self, mount: &str) -> bool {
        self.mount_window(mount).is_none_or(MountWindow::is_empty)
    }

    pub fn ordered_mounts_for_strip(&self, cap: usize) -> Vec<String> {
        let mounts = self.active().ordered_mounts_for_strip(cap);
        if self.hide_idle {
            mounts
                .into_iter()
                .filter(|mount| !self.mount_is_idle(mount))
                .collect()
        } else {
            mounts
        }
    }

    /// Flattened, render-ready tree rows under the active-focus policy,
    /// filtered by the idle-hide toggle. The single source both the
    /// renderer and keyboard navigation read, so a hidden idle mount can
    /// never still be reachable by the cursor.
    pub fn visible_tree_rows(&self) -> Vec<RenderRow> {
        let rows = self
            .forest()
            .render_rows(self.view_now_mono(), ACTIVE_FOCUS_WINDOW_US);
        if self.hide_idle {
            rows.into_iter()
                .filter(|row| !self.mount_is_idle(&row.mount))
                .collect()
        } else {
            rows
        }
    }

    pub fn operation(&self, trace_id: TraceId) -> Option<&Operation> {
        self.active().operation(trace_id)
    }

    pub fn visible_trace_ids(&self) -> Vec<TraceId> {
        self.active().visible_trace_ids(&self.filter)
    }

    /// Number of operations currently retained in memory. Pairs with
    /// [`MAX_RECENT_TRACES`] so subscribers can show eviction pressure.
    pub fn retained_trace_count(&self) -> usize {
        self.active().retained_trace_count()
    }

    pub const fn max_retained_traces() -> usize {
        MAX_RECENT_TRACES
    }

    /// Durable, whole-session counters for the quit receipt. Always reads
    /// the live reducer (never the paused snapshot): quitting while paused
    /// must still report everything that happened.
    pub fn session(&self) -> &SessionStats {
        self.traces.session()
    }

    pub fn apply_record(&mut self, record: &InspectorRecord) {
        // Pausing freezes the *view*, not the reducer: the live state below
        // keeps advancing so nothing that happened while paused is lost,
        // and resuming jumps straight to the caught-up state.
        self.now_mono = record.mono_us;
        self.tick_event_window(record.mono_us);
        self.traces.apply_record(record);
        self.traces.ensure_selected_visible(&self.filter);
        if let Some(frozen) = &mut self.frozen {
            frozen.buffered = frozen.buffered.saturating_add(1);
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

    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.filter.editing {
            self.handle_filter_key(key.code);
            return;
        }
        if let Some(binding) = KEYMAP.iter().find(|binding| (binding.matches)(&key)) {
            (binding.handler)(self, &key);
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

    fn sync_selection_to_tree_cursor(&mut self) {
        let Some(cursor) = self.tree_cursor.clone() else {
            return;
        };
        Self::active_mut(&mut self.traces, &mut self.frozen)
            .select_latest_for_path(&cursor.mount, &cursor.path);
        self.sync_selected_to_live();
    }

    fn toggle_tree_cursor_collapse(&mut self) {
        let Some(cursor) = self.tree_cursor.clone() else {
            return;
        };
        let new_state = Self::active_mut(&mut self.traces, &mut self.frozen)
            .forest
            .toggle_collapsed(&cursor.mount, &cursor.path);
        // Project the resulting flag onto the live forest too (mirrors
        // `sync_selected_to_live`), so a collapse made while paused
        // survives resume instead of reverting to the live forest's
        // untouched state.
        if let Some(collapsed) = new_state {
            self.traces
                .forest
                .set_collapsed(&cursor.mount, &cursor.path, collapsed);
        }
    }

    fn handle_filter_key(&mut self, code: KeyCode) {
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
        // Reset discards retained completed operations from whichever
        // reducer holds them, not just a rendering choice, so — unlike
        // selection/collapse — there's no single "current" identity to
        // compute once and project. Applying it to both independently
        // keeps pause-time reset consistent with live reset: either way,
        // completed ops are gone for good, not resurrected on resume.
        self.traces.reset_recent(&self.filter);
        if let Some(frozen) = &mut self.frozen {
            frozen.traces.reset_recent(&self.filter);
        }
    }

    fn select_next(&mut self) {
        Self::active_mut(&mut self.traces, &mut self.frozen).select_next(&self.filter);
        self.sync_selected_to_live();
    }

    fn select_prev(&mut self) {
        Self::active_mut(&mut self.traces, &mut self.frozen).select_prev(&self.filter);
        self.sync_selected_to_live();
    }

    fn toggle_errors_only(&mut self) {
        self.filter.mode = match self.filter.mode {
            FilterMode::ErrorsOnly => FilterMode::All,
            FilterMode::All => FilterMode::ErrorsOnly,
        };
        Self::active_mut(&mut self.traces, &mut self.frozen).ensure_selected_visible(&self.filter);
        self.sync_selected_to_live();
    }

    fn toggle_idle(&mut self) {
        self.hide_idle = !self.hide_idle;
    }

    fn start_filter_edit(&mut self) {
        self.filter.editing = true;
        self.filter.query.clear();
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

/// One entry in the keymap: the raw-key predicate that selects it, the
/// footer/help label, and the handler it dispatches to. This is the single
/// source of truth for "what does this key do" — `handle_key` dispatches
/// through it and `footer_text` renders it, so the two can't drift the way
/// a hand-written footer string and a hand-written `match` could.
struct KeyBinding {
    matches: fn(&KeyEvent) -> bool,
    label: &'static str,
    description: &'static str,
    handler: fn(&mut App, &KeyEvent),
    /// Handled but intentionally left out of the footer (vim-style aliases
    /// duplicating an already-advertised binding). Still required to be
    /// wired to a real handler, and still covered by the consistency test.
    hidden: bool,
}

fn is_quit(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn is_tab(key: &KeyEvent) -> bool {
    key.code == KeyCode::Tab
}

fn is_pause(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char(' ')
}

fn is_nav(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Up | KeyCode::Down)
}

fn is_collapse(key: &KeyEvent) -> bool {
    key.code == KeyCode::Enter
}

fn is_select_next(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('j' | 'n'))
}

fn is_select_prev(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('k' | 'p'))
}

fn is_toggle_errors(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('e')
}

fn is_toggle_idle(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('i')
}

fn is_filter(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('/')
}

fn is_reset(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('r')
}

fn handle_quit(app: &mut App, _key: &KeyEvent) {
    app.quit = true;
}

fn handle_focus(app: &mut App, _key: &KeyEvent) {
    app.focus = app.focus.cycle();
}

fn handle_pause(app: &mut App, _key: &KeyEvent) {
    app.toggle_pause();
}

fn handle_nav(app: &mut App, key: &KeyEvent) {
    let delta = if key.code == KeyCode::Up { -1 } else { 1 };
    app.move_focus_cursor(delta);
}

fn handle_collapse(app: &mut App, _key: &KeyEvent) {
    if app.focus == PaneFocus::Tree {
        app.toggle_tree_cursor_collapse();
    }
}

fn handle_select_next(app: &mut App, _key: &KeyEvent) {
    app.select_next();
}

fn handle_select_prev(app: &mut App, _key: &KeyEvent) {
    app.select_prev();
}

fn handle_toggle_errors(app: &mut App, _key: &KeyEvent) {
    app.toggle_errors_only();
}

fn handle_toggle_idle(app: &mut App, _key: &KeyEvent) {
    app.toggle_idle();
}

fn handle_filter(app: &mut App, _key: &KeyEvent) {
    app.start_filter_edit();
}

fn handle_reset(app: &mut App, _key: &KeyEvent) {
    app.reset_recent();
}

const KEYMAP: &[KeyBinding] = &[
    KeyBinding {
        matches: is_quit,
        label: "q",
        description: "quit",
        handler: handle_quit,
        hidden: false,
    },
    KeyBinding {
        matches: is_tab,
        label: "tab",
        description: "focus",
        handler: handle_focus,
        hidden: false,
    },
    KeyBinding {
        matches: is_pause,
        label: "space",
        description: "pause",
        handler: handle_pause,
        hidden: false,
    },
    KeyBinding {
        matches: is_nav,
        label: "↑/↓",
        description: "navigate",
        handler: handle_nav,
        hidden: false,
    },
    KeyBinding {
        matches: is_collapse,
        label: "↵",
        description: "collapse",
        handler: handle_collapse,
        hidden: false,
    },
    KeyBinding {
        matches: is_select_next,
        label: "j/n",
        description: "next op",
        handler: handle_select_next,
        hidden: true,
    },
    KeyBinding {
        matches: is_select_prev,
        label: "k/p",
        description: "prev op",
        handler: handle_select_prev,
        hidden: true,
    },
    KeyBinding {
        matches: is_toggle_errors,
        label: "e",
        description: "errors",
        handler: handle_toggle_errors,
        hidden: false,
    },
    KeyBinding {
        matches: is_toggle_idle,
        label: "i",
        description: "idle",
        handler: handle_toggle_idle,
        hidden: false,
    },
    KeyBinding {
        matches: is_filter,
        label: "/",
        description: "filter",
        handler: handle_filter,
        hidden: false,
    },
    KeyBinding {
        matches: is_reset,
        label: "r",
        description: "reset",
        handler: handle_reset,
        hidden: false,
    },
];

/// The header's key-help strip, generated from [`KEYMAP`] so it can never
/// advertise a key with no handler or hide one that has one.
pub fn footer_text() -> String {
    let parts: Vec<String> = KEYMAP
        .iter()
        .filter(|binding| !binding.hidden)
        .map(|binding| format!("{} {}", binding.label, binding.description))
        .collect();
    format!(" {} ", parts.join("  "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn footer_advertises_every_visible_binding() {
        let footer = footer_text();
        for binding in KEYMAP.iter().filter(|binding| !binding.hidden) {
            assert!(
                footer.contains(binding.label),
                "footer missing `{}`: {footer:?}",
                binding.label
            );
        }
    }

    #[test]
    fn every_binding_is_reachable_by_exactly_one_representative_key() {
        // Each binding must own a key nothing else in the table also claims,
        // or dispatch would be ambiguous and the footer/help text would be
        // lying about which key does what. Keeping this table separate from
        // `KEYMAP` (rather than deriving it) is deliberate: it's the
        // independent check that the predicates actually partition the
        // keyboard the way their labels claim.
        let samples: &[(&str, KeyEvent)] = &[
            ("q", key(KeyCode::Char('q'))),
            ("tab", key(KeyCode::Tab)),
            ("space", key(KeyCode::Char(' '))),
            ("↑/↓", key(KeyCode::Up)),
            ("↵", key(KeyCode::Enter)),
            ("j/n", key(KeyCode::Char('j'))),
            ("k/p", key(KeyCode::Char('k'))),
            ("e", key(KeyCode::Char('e'))),
            ("i", key(KeyCode::Char('i'))),
            ("/", key(KeyCode::Char('/'))),
            ("r", key(KeyCode::Char('r'))),
        ];
        assert_eq!(
            samples.len(),
            KEYMAP.len(),
            "every KEYMAP entry needs a representative sample key here"
        );
        for (label, event) in samples {
            let matches: Vec<&str> = KEYMAP
                .iter()
                .filter(|binding| (binding.matches)(event))
                .map(|binding| binding.label)
                .collect();
            assert_eq!(matches, vec![*label], "{event:?}");
        }
    }

    #[test]
    fn pausing_keeps_reducing_but_freezes_the_rendered_view_until_resume() {
        use omnifs_api::events::InspectorEvent;

        fn record(trace_id: TraceId, mono_us: u64, path: &str) -> InspectorRecord {
            InspectorRecord::new(
                "2026-05-23T00:00:00Z",
                mono_us,
                trace_id,
                InspectorEvent::FuseStart {
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: path.into(),
                },
            )
        }

        let mut app = App::new(
            ConnectionMode::Replay,
            "test",
            None,
            "~/omnifs/dns/example.com/A",
        );
        app.apply_record(&record(1, 10, "/before-pause"));
        assert_eq!(app.retained_trace_count(), 1);

        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.paused);

        // Feed more records while paused: the reducer must keep advancing
        // (nothing dropped)...
        app.apply_record(&record(2, 20, "/during-pause-1"));
        app.apply_record(&record(3, 30, "/during-pause-2"));
        assert_eq!(
            app.traces.retained_trace_count(),
            3,
            "live reducer must keep reducing while paused"
        );
        // ...but the rendered/navigable view must not have moved.
        assert_eq!(
            app.retained_trace_count(),
            1,
            "paused view must stay frozen at the pre-pause snapshot"
        );
        assert_eq!(app.buffered_since_pause(), 2);

        app.handle_key(key(KeyCode::Char(' ')));
        assert!(!app.paused);
        assert_eq!(
            app.retained_trace_count(),
            3,
            "resume must render the caught-up state"
        );
        assert_eq!(app.buffered_since_pause(), 0);
    }

    #[test]
    fn pause_time_selection_and_collapse_survive_resume() {
        use omnifs_api::events::InspectorEvent;

        fn record(trace_id: TraceId, mono_us: u64, path: &str) -> InspectorRecord {
            InspectorRecord::new(
                "2026-05-23T00:00:00Z",
                mono_us,
                trace_id,
                InspectorEvent::FuseStart {
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: path.into(),
                },
            )
        }

        let mut app = App::new(
            ConnectionMode::Replay,
            "test",
            None,
            "~/omnifs/dns/example.com/A",
        );
        // Two ops under the same subtree so the ops log has a second
        // entry to select and the tree root has children to hide behind
        // a collapse summary.
        app.apply_record(&record(1, 10, "/dir/one"));
        app.apply_record(&record(2, 20, "/dir/two"));
        // `on_fuse_start` auto-selects the first-ever op.
        assert_eq!(app.selected_trace(), Some(1));

        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.paused);

        // While paused: move the ops-log selection off the pre-pause
        // default, and collapse the mount root so its children fold
        // into a single summary row.
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(
            app.selected_trace(),
            Some(2),
            "selection change must apply to the paused view immediately"
        );
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        let rows_while_paused = app.visible_tree_rows();
        assert!(
            rows_while_paused.iter().any(|row| row
                .path
                .ends_with(super::super::tree::COLLAPSED_SUMMARY_SUFFIX)),
            "collapse must apply to the paused view immediately: {rows_while_paused:?}"
        );

        app.handle_key(key(KeyCode::Char(' ')));
        assert!(!app.paused);

        assert_eq!(
            app.selected_trace(),
            Some(2),
            "selection made while paused must survive resume, not revert to the \
             live reducer's own pre-pause choice"
        );
        let rows_after_resume = app.visible_tree_rows();
        assert!(
            rows_after_resume.iter().any(|row| row
                .path
                .ends_with(super::super::tree::COLLAPSED_SUMMARY_SUFFIX)),
            "collapse made while paused must survive resume: {rows_after_resume:?}"
        );
    }

    #[test]
    fn pause_time_reset_evicts_from_live_and_falls_back_selection_on_disappearance() {
        use omnifs_api::events::{InspectorEvent, OpEnd, OutcomeFields};

        fn start(trace_id: TraceId, mono_us: u64, path: &str) -> InspectorRecord {
            InspectorRecord::new(
                "2026-05-23T00:00:00Z",
                mono_us,
                trace_id,
                InspectorEvent::FuseStart {
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: path.into(),
                },
            )
        }

        fn finish(trace_id: TraceId, mono_us: u64) -> InspectorRecord {
            InspectorRecord::new(
                "2026-05-23T00:00:00Z",
                mono_us,
                trace_id,
                InspectorEvent::FuseEnd {
                    op: "lookup".into(),
                    end: OpEnd {
                        elapsed_us: 10,
                        result: OutcomeFields::ok(),
                    },
                },
            )
        }

        let mut app = App::new(
            ConnectionMode::Replay,
            "test",
            None,
            "~/omnifs/dns/example.com/A",
        );
        // Trace 1 stays running (reset must keep it); trace 2 completes
        // (reset must evict it), matching what `r` already does live.
        app.apply_record(&start(1, 10, "/one"));
        app.apply_record(&start(2, 20, "/two"));
        app.apply_record(&finish(2, 30));

        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.paused);

        // Select the op that reset is about to evict.
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected_trace(), Some(2));

        app.handle_key(key(KeyCode::Char('r')));
        // Reset must actually evict the completed op from the live
        // reducer too, not just the paused view — otherwise resuming
        // would resurrect it, which is exactly the bug this fixes.
        assert_eq!(
            app.traces.retained_trace_count(),
            1,
            "reset while paused must evict completed ops from the live reducer"
        );
        // Its disappearance must trigger the same fallback that already
        // covers a selected row vanishing outside of pause (fall back to
        // the first visible trace), not a new bespoke policy.
        assert_eq!(
            app.selected_trace(),
            Some(1),
            "selection must fall back once its target is evicted by reset"
        );

        app.handle_key(key(KeyCode::Char(' ')));
        assert!(!app.paused);
        assert_eq!(app.selected_trace(), Some(1));
    }

    #[test]
    fn idle_toggle_hides_and_restores_mounts_with_no_recent_activity() {
        use omnifs_api::events::InspectorEvent;

        let mut app = App::new(
            ConnectionMode::Replay,
            "test",
            None,
            "~/omnifs/dns/example.com/A",
        );
        // A `FuseStart` alone puts `github` in the forest (so it's
        // strip-eligible) without ever recording a completion or cache-hit
        // sample. That's exactly the idle case: registered, but nothing in
        // its metrics window, matching the sparkline strip's own "idle"
        // label (`MountWindow::is_empty`).
        app.apply_record(&InspectorRecord::new(
            "2026-05-23T00:00:00Z",
            10,
            1,
            InspectorEvent::FuseStart {
                op: "lookup".into(),
                mount: "github".into(),
                path: "/a".into(),
            },
        ));

        assert!(!app.hide_idle);
        assert!(
            app.ordered_mounts_for_strip(8)
                .contains(&"github".to_string()),
            "mount stays visible before the idle toggle"
        );
        assert!(app.mount_is_idle("github"));

        app.handle_key(key(KeyCode::Char('i')));
        assert!(app.hide_idle);
        assert!(
            !app.ordered_mounts_for_strip(8)
                .contains(&"github".to_string()),
            "idle mount must be hidden from the strip once toggled"
        );

        app.handle_key(key(KeyCode::Char('i')));
        assert!(!app.hide_idle);
        assert!(
            app.ordered_mounts_for_strip(8)
                .contains(&"github".to_string()),
            "toggling back off must restore the mount"
        );
    }
}
