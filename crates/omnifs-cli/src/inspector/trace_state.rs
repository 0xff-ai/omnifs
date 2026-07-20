//! Trace reducer for the inspector TUI.
//!
//! This module owns the state derived from the inspector event stream:
//! retained operations for the waterfall, the live path forest, mount
//! metric windows, palette assignment, sandbox stats, and session stats.

use std::collections::{HashMap, VecDeque};

use omnifs_api::events::{
    CacheKind, CalloutKind, InspectorEvent, InspectorOutcome, InspectorRecord, TraceId,
};

use super::filter::ViewFilter;
use super::format;
use super::metrics::MountWindow;
use super::sandbox::{MountSandboxView, SandboxStats};
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

/// Durable whole-session counters for the quit receipt. Timeline refolds
/// derive their own prefix counters, while [`App`](super::app::App) reads the
/// live reducer so pausing and scrubbing never undercount the session.
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
    /// Fraction of completions served from cache. Uses the same
    /// hit/(hit+completion) definition as [`MountWindow::cache_hit_ratio`].
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

#[derive(Debug, Default)]
pub struct TraceReducer {
    pub forest: MountForest,
    pub palette: MountPalette,
    operations: HashMap<TraceId, Operation>,
    trace_order: VecDeque<TraceId>,
    mount_windows: HashMap<String, MountWindow>,
    sandbox: SandboxStats,
    session: SessionStats,
}

impl TraceReducer {
    /// Durable counters for the whole record sequence folded into this
    /// reducer. Quit receipts use the live reducer, never a scrub prefix.
    pub fn session(&self) -> &SessionStats {
        &self.session
    }

    pub fn mount_window(&self, mount: &str) -> Option<&MountWindow> {
        self.mount_windows.get(mount)
    }

    /// Sandbox port stats for one mount: per-export-method and
    /// per-import-kind windows, open-call counts, and lifetime totals.
    pub fn mount_sandbox(&self, mount: &str) -> Option<MountSandboxView<'_>> {
        self.sandbox.mount_view(mount)
    }

    /// Mounts with any sandbox activity, most recent first.
    pub fn sandbox_mounts_by_activity(&self) -> Vec<&str> {
        self.sandbox.mounts_by_activity()
    }

    pub fn sandbox_active_mount(&self, preferred: Option<&str>) -> Option<&str> {
        self.sandbox.active_mount(preferred)
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
        self.session.events = self.session.events.saturating_add(1);
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
                mount,
                provider,
                method,
                path,
            } => self.on_provider_start(
                trace_id,
                *operation_id,
                mount,
                provider,
                method,
                path,
                record.mono_us,
            ),
            InspectorEvent::ProviderEnd { operation_id, end } => {
                self.on_provider_end(
                    trace_id,
                    *operation_id,
                    end.elapsed_us,
                    end.result.outcome,
                    record.mono_us,
                );
            },
            InspectorEvent::CalloutStart {
                operation_id,
                callout_index,
                kind,
                summary,
            } => self.on_callout_start(
                trace_id,
                *operation_id,
                *callout_index,
                *kind,
                summary,
                record.mono_us,
            ),
            InspectorEvent::CalloutEnd {
                operation_id,
                callout_index,
                end,
            } => self.on_callout_end(
                trace_id,
                *operation_id,
                *callout_index,
                end.elapsed_us,
                end.result.outcome,
                record.mono_us,
            ),
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

    pub fn reset_recent(&mut self) {
        self.operations
            .retain(|_, op| op.status == OperationStatus::Running);
        self.trace_order
            .retain(|id| self.operations.contains_key(id));
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
        self.session.completions = self.session.completions.saturating_add(1);
        if outcome_status(outcome) == OperationStatus::Error {
            self.session.errors = self.session.errors.saturating_add(1);
        }
        if self
            .session
            .slowest
            .as_ref()
            .is_none_or(|previous| elapsed_us > previous.elapsed_us)
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

    // The parameter list mirrors the wire event's fields one-to-one; a
    // params struct would just restate `InspectorEvent::ProviderStart`.
    #[allow(clippy::too_many_arguments)]
    fn on_provider_start(
        &mut self,
        trace_id: TraceId,
        operation_id: u64,
        mount: &str,
        provider: &str,
        method: &str,
        path: &str,
        mono_us: u64,
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
        // Unlike provider.end and the callout events, provider.start
        // carries its own mount, so sandbox stats don't need the
        // operation log to resolve it and stay correct even if the
        // matching fuse.start was already evicted.
        self.sandbox
            .on_provider_start(mount, trace_id, operation_id, provider, method, mono_us);
    }

    fn on_provider_end(
        &mut self,
        trace_id: TraceId,
        operation_id: u64,
        elapsed_us: u64,
        outcome: InspectorOutcome,
        mono_us: u64,
    ) {
        if let Some(operation) = self.operations.get_mut(&trace_id) {
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
        self.sandbox
            .on_provider_end(trace_id, operation_id, elapsed_us, outcome, mono_us);
    }

    fn on_callout_start(
        &mut self,
        trace_id: TraceId,
        operation_id: u64,
        callout_index: u32,
        kind: CalloutKind,
        summary: &str,
        mono_us: u64,
    ) {
        // Callout events carry no mount; resolve it through the trace's
        // operation. If the trace was already evicted there's nothing to
        // correlate the callout to, so skip silently rather than guess.
        let Some(operation) = self.operations.get_mut(&trace_id) else {
            return;
        };
        let mount = operation.mount.clone();
        operation.stages.push(Stage::in_flight(
            StageKind::Callout(callout_index),
            format!("{kind} {summary}"),
        ));
        self.sandbox.on_callout_start(
            &mount,
            trace_id,
            operation_id,
            callout_index,
            kind,
            summary,
            mono_us,
        );
    }

    fn on_callout_end(
        &mut self,
        trace_id: TraceId,
        operation_id: u64,
        callout_index: u32,
        elapsed_us: u64,
        outcome: InspectorOutcome,
        mono_us: u64,
    ) {
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation.close_in_flight(
                |s| matches!(&s.kind, StageKind::Callout(idx) if *idx == callout_index),
                elapsed_us,
                outcome,
            );
        }
        self.sandbox.on_callout_end(
            trace_id,
            operation_id,
            callout_index,
            elapsed_us,
            outcome,
            mono_us,
        );
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
            self.session.cache_hits = self.session.cache_hits.saturating_add(1);
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
        self.sandbox.remove_trace(trace_id);
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
    use std::collections::HashSet;

    use super::super::sandbox::PortId;
    use super::*;
    use omnifs_api::events::{OpEnd, OutcomeFields};

    fn record(trace_id: TraceId, mono_us: u64, event: InspectorEvent) -> InspectorRecord {
        InspectorRecord::new("2026-05-23T12:00:00Z", mono_us, trace_id, event)
    }

    fn ok_end(elapsed_us: u64) -> OpEnd {
        OpEnd {
            elapsed_us,
            result: OutcomeFields::ok(),
        }
    }

    fn provider_start(
        trace_id: TraceId,
        mono_us: u64,
        operation_id: u64,
        mount: &str,
        method: &str,
    ) -> InspectorRecord {
        record(
            trace_id,
            mono_us,
            InspectorEvent::ProviderStart {
                operation_id,
                mount: mount.into(),
                provider: mount.into(),
                method: method.into(),
                path: "/x".into(),
            },
        )
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
            provider_start(7, 20, 42, "github", "lookup_child"),
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
                    end: ok_end(1_200),
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
                    end: ok_end(2_500),
                },
            ),
            record(
                7,
                70,
                InspectorEvent::FuseEnd {
                    op: "lookup".into(),
                    end: ok_end(3_000),
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
        let rows = traces.forest.render_rows(10, 30_000_000, &HashSet::new());
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
        let rows = traces.forest.render_rows(20, 30_000_000, &HashSet::new());
        let missing = rows
            .iter()
            .find(|row| row.path == "notifications/missing")
            .expect("missing path row");
        assert_eq!(missing.status, super::super::tree::NodeStatus::Miss);
    }

    #[test]
    fn sandbox_stats_track_lifecycle_activity_and_eviction() {
        let mut traces = TraceReducer::default();
        let export = PortId::Export("lookup_child".into());
        let import = PortId::Import(CalloutKind::Fetch);
        for event in [
            record(
                11,
                10,
                InspectorEvent::FuseStart {
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: "/raulk/omnifs".into(),
                },
            ),
            provider_start(11, 20, 55, "github", "lookup_child"),
            record(
                11,
                30,
                InspectorEvent::CalloutStart {
                    operation_id: 55,
                    callout_index: 0,
                    kind: CalloutKind::Fetch,
                    summary: "GET api.github.com/repos/raulk/omnifs".into(),
                },
            ),
        ] {
            traces.apply_record(&event);
        }

        let sandbox = traces.mount_sandbox("github").expect("mount sandbox");
        assert_eq!(sandbox.open_count(&export), 1);
        assert_eq!(sandbox.open_count(&import), 1);
        assert_eq!(
            (sandbox.total_open_exports(), sandbox.total_open_imports()),
            (1, 1)
        );

        for event in [
            record(
                11,
                40,
                InspectorEvent::CalloutEnd {
                    operation_id: 55,
                    callout_index: 0,
                    end: ok_end(1_200),
                },
            ),
            record(
                11,
                50,
                InspectorEvent::ProviderEnd {
                    operation_id: 55,
                    end: ok_end(2_000),
                },
            ),
        ] {
            traces.apply_record(&event);
        }

        let sandbox = traces.mount_sandbox("github").expect("mount sandbox");
        assert_eq!(
            (sandbox.total_open_exports(), sandbox.total_open_imports()),
            (0, 0)
        );
        for port in [&export, &import] {
            let stats = sandbox.port_stats(port).expect("port stats");
            assert_eq!(stats.lifetime, 1);
            assert!(!stats.window.is_empty());
        }

        traces.apply_record(&provider_start(12, 70, 7, "scratch", "read_file"));
        assert_eq!(
            traces.sandbox_mounts_by_activity(),
            vec!["scratch", "github"]
        );
        assert_eq!(
            traces
                .mount_sandbox("scratch")
                .expect("mount sandbox")
                .total_open_exports(),
            1
        );
        traces.evict_trace(12);
        assert_eq!(
            traces
                .mount_sandbox("scratch")
                .expect("mount sandbox")
                .total_open_exports(),
            0
        );
    }

    #[test]
    fn completion_settles_sandbox_after_operation_eviction() {
        let mut traces = TraceReducer::default();
        traces.apply_record(&provider_start(120, 10, 7, "github", "lookup_child"));
        traces.operations.remove(&120);
        traces.apply_record(&record(
            120,
            20,
            InspectorEvent::ProviderEnd {
                operation_id: 7,
                end: ok_end(10),
            },
        ));
        let sandbox = traces.mount_sandbox("github").expect("sandbox");
        assert_eq!(sandbox.total_open_exports(), 0);
        let stats = sandbox
            .port_stats(&PortId::Export("lookup_child".into()))
            .unwrap();
        assert_eq!(stats.lifetime, 1);
        assert!(!stats.window.is_empty());
    }
}
