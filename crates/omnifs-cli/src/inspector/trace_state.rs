//! Trace reducer for the inspector TUI.
//!
//! This module owns the state derived from the inspector event stream:
//! retained operations for the waterfall, the live path forest, mount
//! metric windows, palette assignment, and operation selection.

use std::collections::{HashMap, VecDeque};

use omnifs_api::events::{
    CacheKind, CalloutKind, InspectorEvent, InspectorOutcome, InspectorRecord, TraceId,
};

use super::filter::ViewFilter;
use super::format;
use super::metrics::MountWindow;
use super::tree::MountForest;
use ratatui::style::Color;

/// Hard cap on retained per-trace operation records. The previous
/// value (256) was sized for a quiet observability stream and got
/// evicted within seconds by routine FUSE traffic, making operations
/// the user was looking at silently disappear. 4096 traces x a few
/// KB each is still trivial in absolute memory but covers a typical
/// debugging session without GC pressure.
pub const MAX_RECENT_TRACES: usize = 4096;

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

/// Mount color palette. First-sight assignment from a curated list; cycles
/// deterministically when exceeded so screenshots remain stable across
/// reorderings.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatus {
    Running,
    Ok,
    Error,
}

/// Typed kind of a [`Stage`]. Carries enough data to render the
/// canonical `subsystem.event` display label and to match stages by
/// identity.
#[derive(Debug, Clone)]
pub enum StageKind {
    Fuse(String),
    Provider(String),
    Callout(u32),
    Cache(CacheKind),
    SubtreeStart,
    SubtreeEnd,
    CloneStart,
    CloneEnd,
}

impl StageKind {
    /// Canonical `subsystem.event` token for display. Cache stages
    /// route through the shared [`super::format::cache_event_label`]
    /// so the plain-mode and TUI surfaces agree.
    pub fn display_label(&self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow;
        match self {
            Self::Fuse(op) => Cow::Owned(format!("fuse.{op}")),
            Self::Provider(method) => Cow::Owned(format!("provider.{method}")),
            Self::Callout(idx) => Cow::Owned(format!("callout.{idx}")),
            Self::Cache(kind) => Cow::Borrowed(super::format::cache_event_label(*kind)),
            Self::SubtreeStart => Cow::Borrowed("subtree.start"),
            Self::SubtreeEnd => Cow::Borrowed("subtree.end"),
            Self::CloneStart => Cow::Borrowed("clone.start"),
            Self::CloneEnd => Cow::Borrowed("clone.end"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Stage {
    pub kind: StageKind,
    pub detail: String,
    pub elapsed_us: Option<u64>,
    pub outcome: Option<InspectorOutcome>,
    pub in_flight: bool,
}

impl Stage {
    fn in_flight(kind: StageKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            elapsed_us: None,
            outcome: None,
            in_flight: true,
        }
    }

    fn done(
        kind: StageKind,
        detail: impl Into<String>,
        elapsed_us: Option<u64>,
        outcome: Option<InspectorOutcome>,
    ) -> Self {
        Self {
            kind,
            detail: detail.into(),
            elapsed_us,
            outcome,
            in_flight: false,
        }
    }
}

/// Durable per-session counters, untouched by `reset_recent` or pause: the
/// quit receipt needs "what happened this whole session," not "what's
/// currently retained in the waterfall."
#[derive(Debug, Default, Clone)]
pub struct SessionStats {
    pub events: u64,
    completions: u64,
    pub errors: u64,
    cache_hits: u64,
    pub slowest: Option<SlowOp>,
}

/// The single slowest completed FUSE operation seen this session.
#[derive(Debug, Clone)]
pub struct SlowOp {
    pub mount: String,
    pub path: String,
    pub op: String,
    pub elapsed_us: u64,
}

impl SessionStats {
    /// Fraction of completions served from cache. Mirrors
    /// [`MountWindow::cache_hit_ratio`]'s hit/(hit+completion) definition so
    /// the quit receipt's number means the same thing the live sparkline's
    /// per-mount number does; `None` when nothing completed yet.
    #[allow(clippy::cast_precision_loss)]
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        let total = self.cache_hits + self.completions;
        if total == 0 {
            return None;
        }
        Some(self.cache_hits as f64 / total as f64)
    }
}

#[derive(Debug, Clone)]
pub struct Operation {
    pub trace_id: TraceId,
    pub fuse_op: String,
    pub mount: String,
    pub path: String,
    pub provider_name: Option<String>,
    pub provider_method: Option<String>,
    pub provider_ops: Vec<u64>,
    pub stages: Vec<Stage>,
    pub status: OperationStatus,
    pub outcome: Option<InspectorOutcome>,
    pub started_mono: u64,
    pub ended_mono: Option<u64>,
    pub fuse_elapsed_us: Option<u64>,
}

impl Operation {
    fn new(trace_id: TraceId, mount: String, path: String, fuse_op: &str, mono_us: u64) -> Self {
        Self {
            trace_id,
            fuse_op: fuse_op.to_string(),
            mount,
            path,
            provider_name: None,
            provider_method: None,
            provider_ops: Vec::new(),
            stages: vec![Stage::in_flight(StageKind::Fuse(fuse_op.to_string()), "")],
            status: OperationStatus::Running,
            outcome: None,
            started_mono: mono_us,
            ended_mono: None,
            fuse_elapsed_us: None,
        }
    }

    fn finalize_fuse(
        &mut self,
        op: &str,
        elapsed_us: u64,
        outcome: InspectorOutcome,
        mono_us: u64,
    ) {
        self.fuse_elapsed_us = Some(elapsed_us);
        self.ended_mono = Some(mono_us);
        if let Some(stage) = self.stages.first_mut() {
            stage.kind = StageKind::Fuse(op.to_string());
            stage.elapsed_us = Some(elapsed_us);
            stage.outcome = Some(outcome);
            stage.in_flight = false;
        }
        self.status = outcome_status(outcome);
        self.outcome = Some(outcome);
    }

    fn close_in_flight<F>(&mut self, predicate: F, elapsed_us: u64, outcome: InspectorOutcome)
    where
        F: Fn(&Stage) -> bool,
    {
        if let Some(stage) = self.stages.iter_mut().rev().find(|s| predicate(s)) {
            stage.elapsed_us = Some(elapsed_us);
            stage.outcome = Some(outcome);
            stage.in_flight = false;
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct TraceReducer {
    pub forest: MountForest,
    pub palette: MountPalette,
    selected: Option<TraceId>,
    operations: HashMap<TraceId, Operation>,
    trace_order: VecDeque<TraceId>,
    mount_windows: HashMap<String, MountWindow>,
    session: SessionStats,
}

impl TraceReducer {
    pub fn selected(&self) -> Option<TraceId> {
        self.selected
    }

    /// Force the selection to an exact identity, bypassing the usual
    /// next/prev/visibility computation. Lets a choice made against one
    /// reducer instance (e.g. the paused snapshot) be projected onto
    /// another (the live reducer) at the same trace identity, so the two
    /// don't drift apart while the view is frozen.
    pub fn set_selected(&mut self, trace_id: Option<TraceId>) {
        self.selected = trace_id;
    }

    /// Durable session counters. Unlike every other accessor here, callers
    /// needing the quit receipt should read this off the live reducer, not
    /// a paused snapshot: pausing must not make the receipt undercount.
    pub fn session(&self) -> &SessionStats {
        &self.session
    }

    pub fn mount_window(&self, mount: &str) -> Option<&MountWindow> {
        self.mount_windows.get(mount)
    }

    pub fn ordered_mounts_for_strip(&self, cap: usize) -> Vec<String> {
        let mut mounts: Vec<_> = self.forest.iter().collect();
        mounts.sort_by_key(|tree| std::cmp::Reverse(tree.last_activity_mono));
        mounts
            .into_iter()
            .take(cap)
            .map(|tree| tree.mount.clone())
            .collect()
    }

    pub fn operation(&self, trace_id: TraceId) -> Option<&Operation> {
        self.operations.get(&trace_id)
    }

    pub fn visible_trace_ids(&self, filter: &ViewFilter) -> Vec<TraceId> {
        self.trace_order
            .iter()
            .copied()
            .filter(|id| self.trace_visible(*id, filter))
            .collect()
    }

    /// Number of operations currently retained in memory. Pairs with
    /// [`MAX_RECENT_TRACES`] so subscribers can show eviction pressure.
    pub fn retained_trace_count(&self) -> usize {
        self.trace_order.len()
    }

    pub fn apply_record(&mut self, record: &InspectorRecord) {
        self.session.events += 1;
        let trace_id = record.trace_id;
        match &record.event {
            InspectorEvent::FuseStart { op, mount, path } => {
                self.on_fuse_start(trace_id, op, mount, path, record.mono_us);
            },
            InspectorEvent::FuseEnd { op, end } => self.on_fuse_end(
                trace_id,
                op,
                end.elapsed_us,
                end.result.outcome,
                record.mono_us,
            ),
            InspectorEvent::ProviderStart {
                operation_id,
                provider,
                method,
                path,
                ..
            } => self.on_provider_start(trace_id, *operation_id, provider, method, path),
            InspectorEvent::ProviderEnd { end, .. } => {
                self.on_provider_end(trace_id, end.elapsed_us, end.result.outcome);
            },
            InspectorEvent::CalloutStart {
                callout_index,
                kind,
                summary,
                ..
            } => self.on_callout_start(trace_id, *callout_index, *kind, summary),
            InspectorEvent::CalloutEnd {
                callout_index, end, ..
            } => self.on_callout_end(trace_id, *callout_index, end.elapsed_us, end.result.outcome),
            InspectorEvent::CacheEvent {
                mount,
                path,
                kind,
                elapsed_us,
                ..
            } => self.on_cache(trace_id, mount, path, *kind, *elapsed_us, record.mono_us),
            InspectorEvent::SubtreeStart { tree_ref, .. } => {
                self.on_subtree_start(trace_id, tree_ref, record.mono_us);
            },
            InspectorEvent::SubtreeEnd { tree_ref, end, .. } => {
                self.on_subtree_end(trace_id, tree_ref, end.elapsed_us, end.result.outcome);
            },
            InspectorEvent::CloneStart {
                cache_key, remote, ..
            } => self.on_clone_start(trace_id, cache_key, remote),
            InspectorEvent::CloneEnd { cache_key, end, .. } => {
                self.on_clone_end(trace_id, cache_key, end.elapsed_us, end.result.outcome);
            },
        }
    }

    pub fn reset_recent(&mut self, filter: &ViewFilter) {
        self.operations
            .retain(|_, op| op.status == OperationStatus::Running);
        self.trace_order
            .retain(|id| self.operations.contains_key(id));
        self.ensure_selected_visible(filter);
    }

    pub fn select_next(&mut self, filter: &ViewFilter) {
        let visible = self.visible_trace_ids(filter);
        if visible.is_empty() {
            return;
        }
        let idx = self
            .selected
            .and_then(|sel| visible.iter().position(|id| *id == sel))
            .map_or(0, |i| (i + 1).min(visible.len() - 1));
        self.selected = Some(visible[idx]);
    }

    pub fn select_prev(&mut self, filter: &ViewFilter) {
        let visible = self.visible_trace_ids(filter);
        if visible.is_empty() {
            return;
        }
        let idx = self
            .selected
            .and_then(|sel| visible.iter().position(|id| *id == sel))
            .map_or(0, |i| i.saturating_sub(1));
        self.selected = Some(visible[idx]);
    }

    pub fn select_latest_for_path(&mut self, mount: &str, path: &str) {
        let mut best: Option<(u64, TraceId)> = None;
        for op in self.operations.values() {
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
                best = Some((ts, op.trace_id));
            }
        }
        if let Some((_, trace_id)) = best {
            self.selected = Some(trace_id);
        }
    }

    pub fn ensure_selected_visible(&mut self, filter: &ViewFilter) {
        let selected_is_visible = self
            .selected
            .is_some_and(|id| self.operation(id).is_some_and(|op| filter.matches(op)));
        if !selected_is_visible {
            self.selected = self.visible_trace_ids(filter).first().copied();
        }
    }

    fn on_fuse_start(
        &mut self,
        trace_id: TraceId,
        op: &str,
        mount: &str,
        path: &str,
        mono_us: u64,
    ) {
        let normalized_path = self.forest.mount_tree_mut(mount).normalize_path(path);
        let operation = Operation::new(
            trace_id,
            mount.to_string(),
            normalized_path.clone(),
            op,
            mono_us,
        );
        self.operations.insert(trace_id, operation);
        self.push_trace(trace_id);
        self.palette.color_for(mount);
        self.forest
            .mount_tree_mut(mount)
            .mark_in_flight(&normalized_path, trace_id, mono_us);
        if self.selected.is_none() {
            self.selected = Some(trace_id);
        }
    }

    fn on_fuse_end(
        &mut self,
        trace_id: TraceId,
        op: &str,
        elapsed_us: u64,
        outcome: InspectorOutcome,
        mono_us: u64,
    ) {
        let Some(operation) = self.operations.get_mut(&trace_id) else {
            return;
        };
        operation.finalize_fuse(op, elapsed_us, outcome, mono_us);
        let mount = operation.mount.clone();
        let path = operation.path.clone();
        self.mount_windows
            .entry(mount.clone())
            .or_default()
            .record_completion(mono_us, elapsed_us, outcome);
        self.session.completions += 1;
        if outcome_status(outcome) == OperationStatus::Error {
            self.session.errors += 1;
        }
        if self
            .session
            .slowest
            .as_ref()
            .is_none_or(|prev| elapsed_us > prev.elapsed_us)
        {
            self.session.slowest = Some(SlowOp {
                mount: mount.clone(),
                path: path.clone(),
                op: op.to_string(),
                elapsed_us,
            });
        }
        // A negative lookup is expected while browsing a projected namespace;
        // keep it in the operation log, but don't make the path look like a
        // permanent hard failure in the activity tree.
        if outcome == InspectorOutcome::NotFound {
            self.forest
                .mount_tree_mut(&mount)
                .complete_miss(&path, trace_id, elapsed_us, mono_us);
        } else {
            self.forest.mount_tree_mut(&mount).complete(
                &path,
                trace_id,
                elapsed_us,
                outcome == InspectorOutcome::Ok,
                mono_us,
            );
        }
    }

    fn on_provider_start(
        &mut self,
        trace_id: TraceId,
        operation_id: u64,
        provider: &str,
        method: &str,
        path: &str,
    ) {
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation.provider_name = Some(provider.to_string());
            operation.provider_method = Some(method.to_string());
            if !operation.provider_ops.contains(&operation_id) {
                operation.provider_ops.push(operation_id);
            }
            if operation.path.is_empty() {
                path.clone_into(&mut operation.path);
            }
            operation.stages.push(Stage::in_flight(
                StageKind::Provider(method.to_string()),
                "",
            ));
        }
    }

    fn on_provider_end(&mut self, trace_id: TraceId, elapsed_us: u64, outcome: InspectorOutcome) {
        let Some(operation) = self.operations.get_mut(&trace_id) else {
            return;
        };
        let method = operation.provider_method.clone();
        operation.close_in_flight(
            |s| {
                s.in_flight
                    && matches!(&s.kind, StageKind::Provider(m) if Some(m.as_str()) == method.as_deref())
            },
            elapsed_us,
            outcome,
        );
    }

    fn on_callout_start(
        &mut self,
        trace_id: TraceId,
        callout_index: u32,
        kind: CalloutKind,
        summary: &str,
    ) {
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation.stages.push(Stage::in_flight(
                StageKind::Callout(callout_index),
                format!("{kind} {summary}"),
            ));
        }
    }

    fn on_callout_end(
        &mut self,
        trace_id: TraceId,
        callout_index: u32,
        elapsed_us: u64,
        outcome: InspectorOutcome,
    ) {
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation.close_in_flight(
                |s| matches!(&s.kind, StageKind::Callout(idx) if *idx == callout_index),
                elapsed_us,
                outcome,
            );
        }
    }

    fn on_cache(
        &mut self,
        trace_id: TraceId,
        mount: &str,
        path: &str,
        kind: CacheKind,
        elapsed_us: Option<u64>,
        mono_us: u64,
    ) {
        let normalized_path = self.forest.mount_tree_mut(mount).normalize_path(path);
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation.stages.push(Stage {
                kind: StageKind::Cache(kind),
                detail: format::shorten_path(&normalized_path, 48),
                elapsed_us,
                outcome: Some(InspectorOutcome::Ok),
                in_flight: false,
            });
        }
        self.palette.color_for(mount);
        let is_hit = matches!(
            kind,
            CacheKind::BrowseHit | CacheKind::FileHit | CacheKind::BlobHit
        );
        if is_hit {
            self.mount_windows
                .entry(mount.to_string())
                .or_default()
                .record_cache_hit(mono_us);
            self.forest
                .mount_tree_mut(mount)
                .cache_hit(&normalized_path, mono_us);
            self.session.cache_hits += 1;
        }
    }

    fn on_subtree_start(&mut self, trace_id: TraceId, tree_ref: &str, mono_us: u64) {
        let location = self
            .operations
            .get(&trace_id)
            .map(|op| (op.mount.clone(), op.path.clone()));
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation
                .stages
                .push(Stage::in_flight(StageKind::SubtreeStart, tree_ref));
        }
        if let Some((mount, path)) = location {
            self.forest
                .mount_tree_mut(&mount)
                .mark_subtree_handoff(&path, mono_us);
        }
    }

    fn on_subtree_end(
        &mut self,
        trace_id: TraceId,
        tree_ref: &str,
        elapsed_us: u64,
        outcome: InspectorOutcome,
    ) {
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation.stages.push(Stage::done(
                StageKind::SubtreeEnd,
                tree_ref,
                Some(elapsed_us),
                Some(outcome),
            ));
        }
    }

    fn on_clone_start(&mut self, trace_id: TraceId, cache_key: &str, remote: &str) {
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation.stages.push(Stage::in_flight(
                StageKind::CloneStart,
                format!("{cache_key} {remote}"),
            ));
        }
    }

    fn on_clone_end(
        &mut self,
        trace_id: TraceId,
        cache_key: &str,
        elapsed_us: u64,
        outcome: InspectorOutcome,
    ) {
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation.stages.push(Stage::done(
                StageKind::CloneEnd,
                cache_key,
                Some(elapsed_us),
                Some(outcome),
            ));
        }
    }

    fn push_trace(&mut self, trace_id: TraceId) {
        self.trace_order.retain(|id| *id != trace_id);
        self.trace_order.push_front(trace_id);
        while self.trace_order.len() > MAX_RECENT_TRACES {
            if let Some(evicted) = self.trace_order.pop_back() {
                self.evict_trace(evicted);
            }
        }
    }

    fn evict_trace(&mut self, trace_id: TraceId) {
        self.operations.remove(&trace_id);
        if self.selected == Some(trace_id) {
            self.selected = None;
        }
    }

    fn trace_visible(&self, trace_id: TraceId, filter: &ViewFilter) -> bool {
        let Some(operation) = self.operations.get(&trace_id) else {
            return false;
        };
        filter.matches(operation)
    }
}

fn outcome_status(outcome: InspectorOutcome) -> OperationStatus {
    if outcome == InspectorOutcome::Ok {
        OperationStatus::Ok
    } else {
        OperationStatus::Error
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::events::{OpEnd, OutcomeFields};

    fn record(trace_id: TraceId, mono_us: u64, event: InspectorEvent) -> InspectorRecord {
        InspectorRecord::new("2026-05-23T12:00:00Z", mono_us, trace_id, event)
    }

    #[test]
    fn synthetic_trace_builds_fixed_waterfall() {
        let mut traces = TraceReducer::default();
        let events = [
            record(
                7,
                10,
                InspectorEvent::FuseStart {
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: "/raulk/omnifs".into(),
                },
            ),
            record(
                7,
                20,
                InspectorEvent::ProviderStart {
                    operation_id: 42,
                    mount: "github".into(),
                    provider: "github".into(),
                    method: "lookup_child".into(),
                    path: "/raulk/omnifs".into(),
                },
            ),
            record(
                7,
                30,
                InspectorEvent::CalloutStart {
                    operation_id: 42,
                    callout_index: 0,
                    kind: CalloutKind::Fetch,
                    summary: "GET api.github.com/repos/raulk/omnifs".into(),
                },
            ),
            record(
                7,
                40,
                InspectorEvent::CalloutEnd {
                    operation_id: 42,
                    callout_index: 0,
                    end: OpEnd {
                        elapsed_us: 1_200,
                        result: OutcomeFields::ok(),
                    },
                },
            ),
            record(
                7,
                50,
                InspectorEvent::CacheEvent {
                    operation_id: Some(42),
                    mount: "github".into(),
                    path: "/raulk/omnifs".into(),
                    kind: CacheKind::BrowseHit,
                    elapsed_us: None,
                },
            ),
            record(
                7,
                60,
                InspectorEvent::ProviderEnd {
                    operation_id: 42,
                    end: OpEnd {
                        elapsed_us: 2_500,
                        result: OutcomeFields::ok(),
                    },
                },
            ),
            record(
                7,
                70,
                InspectorEvent::FuseEnd {
                    op: "lookup".into(),
                    end: OpEnd {
                        elapsed_us: 3_000,
                        result: OutcomeFields::ok(),
                    },
                },
            ),
        ];

        for event in events {
            traces.apply_record(&event);
        }

        let op = traces.operation(7).expect("trace 7");
        let labels: Vec<_> = op
            .stages
            .iter()
            .map(|stage| stage.kind.display_label().into_owned())
            .collect();
        assert_eq!(
            labels,
            vec![
                "fuse.lookup",
                "provider.lookup_child",
                "callout.0",
                "cache.hit"
            ]
        );
        assert_eq!(op.status, OperationStatus::Ok);
        assert_eq!(op.fuse_elapsed_us, Some(3_000));
        assert_eq!(op.provider_name.as_deref(), Some("github"));
        assert!(op.stages.iter().all(|stage| !stage.in_flight));
    }

    #[test]
    fn duplicate_mount_root_is_normalized_in_tree_and_operation() {
        let mut traces = TraceReducer::default();
        traces.apply_record(&record(
            8,
            10,
            InspectorEvent::FuseStart {
                op: "lookup".into(),
                mount: "github".into(),
                path: "/github/notifications".into(),
            },
        ));
        let op = traces.operation(8).expect("trace");
        assert_eq!(op.path, "/notifications");
        let rows = traces.forest.render_rows(10, 30_000_000);
        let paths: Vec<_> = rows.iter().map(|row| row.path.as_str()).collect();
        assert!(paths.contains(&"notifications"));
        assert!(!paths.contains(&"github/notifications"));
    }

    #[test]
    fn not_found_is_retained_in_log_but_benign_in_tree() {
        let mut traces = TraceReducer::default();
        traces.apply_record(&record(
            9,
            10,
            InspectorEvent::FuseStart {
                op: "lookup".into(),
                mount: "github".into(),
                path: "/notifications/missing".into(),
            },
        ));
        traces.apply_record(&record(
            9,
            20,
            InspectorEvent::FuseEnd {
                op: "lookup".into(),
                end: OpEnd {
                    elapsed_us: 10,
                    result: OutcomeFields::with_outcome(InspectorOutcome::NotFound),
                },
            },
        ));
        assert_eq!(
            traces.operation(9).expect("trace").outcome,
            Some(InspectorOutcome::NotFound)
        );
        let rows = traces.forest.render_rows(20, 30_000_000);
        let missing = rows
            .iter()
            .find(|row| row.path == "notifications/missing")
            .expect("missing path row");
        assert_eq!(missing.status, super::super::tree::NodeStatus::Miss);
    }
}
