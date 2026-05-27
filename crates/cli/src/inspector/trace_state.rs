//! Trace reducer for the inspector TUI.
//!
//! This module owns the state derived from the inspector event stream:
//! retained operations for the waterfall, the live path forest, mount
//! metric windows, palette assignment, and operation selection.

use std::collections::{HashMap, VecDeque};

use omnifs_inspector::{
    CacheKind, CalloutKind, InspectorEvent, InspectorOutcome, InspectorRecord, TraceId,
};

use super::filter::ViewFilter;
use super::metrics::MountWindow;
use super::palette::MountPalette;
use super::scene;
use super::tree::MountForest;

/// Hard cap on retained per-trace operation records. The previous
/// value (256) was sized for a quiet observability stream and got
/// evicted within seconds by routine FUSE traffic, making operations
/// the user was looking at silently disappear. 4096 traces x a few
/// KB each is still trivial in absolute memory but covers a typical
/// debugging session without GC pressure.
pub const MAX_RECENT_TRACES: usize = 4096;

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
    ProviderSuspend,
    ProviderResume,
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
            Self::ProviderSuspend => Cow::Borrowed("provider.suspend"),
            Self::ProviderResume => Cow::Borrowed("provider.resume"),
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

#[derive(Debug, Clone)]
pub struct Operation {
    pub trace_id: TraceId,
    pub fuse_op: String,
    pub mount: String,
    pub path: String,
    pub provider_id: Option<String>,
    pub provider_method: Option<String>,
    pub provider_ops: Vec<u64>,
    pub stages: Vec<Stage>,
    pub status: OperationStatus,
    pub outcome: Option<InspectorOutcome>,
    pub started_mono: u64,
    pub ended_mono: Option<u64>,
    pub fuse_elapsed_us: Option<u64>,
    pub provider_suspended: bool,
}

impl Operation {
    fn new(trace_id: TraceId, mount: String, path: String, fuse_op: &str, mono_us: u64) -> Self {
        Self {
            trace_id,
            fuse_op: fuse_op.to_string(),
            mount,
            path,
            provider_id: None,
            provider_method: None,
            provider_ops: Vec::new(),
            stages: vec![Stage::in_flight(StageKind::Fuse(fuse_op.to_string()), "")],
            status: OperationStatus::Running,
            outcome: None,
            started_mono: mono_us,
            ended_mono: None,
            fuse_elapsed_us: None,
            provider_suspended: false,
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
    selected: Option<TraceId>,
    operations: HashMap<TraceId, Operation>,
    trace_order: VecDeque<TraceId>,
    mount_windows: HashMap<String, MountWindow>,
}

impl TraceReducer {
    pub fn selected(&self) -> Option<TraceId> {
        self.selected
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
        match &record.event {
            InspectorEvent::FuseStart {
                trace_id,
                op,
                mount,
                path,
            } => self.on_fuse_start(*trace_id, op, mount, path, record.mono_us),
            InspectorEvent::FuseEnd {
                trace_id,
                op,
                elapsed_us,
                result,
            } => self.on_fuse_end(*trace_id, op, *elapsed_us, result.outcome, record.mono_us),
            InspectorEvent::ProviderStart {
                trace_id,
                operation_id,
                provider,
                method,
                path,
                ..
            } => self.on_provider_start(*trace_id, *operation_id, provider, method, path),
            InspectorEvent::ProviderSuspend {
                trace_id,
                callout_count,
                ..
            } => self.on_provider_suspend(*trace_id, *callout_count),
            InspectorEvent::ProviderResume {
                trace_id,
                round,
                result_count,
                ..
            } => self.on_provider_resume(*trace_id, *round, *result_count),
            InspectorEvent::ProviderEnd {
                trace_id,
                elapsed_us,
                result,
                ..
            } => self.on_provider_end(*trace_id, *elapsed_us, result.outcome),
            InspectorEvent::CalloutStart {
                trace_id,
                callout_index,
                kind,
                summary,
                ..
            } => self.on_callout_start(*trace_id, *callout_index, *kind, summary),
            InspectorEvent::CalloutEnd {
                trace_id,
                callout_index,
                elapsed_us,
                result,
                ..
            } => self.on_callout_end(*trace_id, *callout_index, *elapsed_us, result.outcome),
            InspectorEvent::CacheEvent {
                trace_id,
                mount,
                path,
                kind,
                elapsed_us,
                ..
            } => self.on_cache(*trace_id, mount, path, *kind, *elapsed_us, record.mono_us),
            InspectorEvent::SubtreeStart {
                trace_id, tree_ref, ..
            } => self.on_subtree_start(*trace_id, tree_ref, record.mono_us),
            InspectorEvent::SubtreeEnd {
                trace_id,
                tree_ref,
                elapsed_us,
                result,
                ..
            } => self.on_subtree_end(*trace_id, tree_ref, *elapsed_us, result.outcome),
            InspectorEvent::CloneStart {
                trace_id,
                cache_key,
                remote,
                ..
            } => self.on_clone_start(*trace_id, cache_key, remote),
            InspectorEvent::CloneEnd {
                trace_id,
                cache_key,
                elapsed_us,
                result,
                ..
            } => self.on_clone_end(*trace_id, cache_key, *elapsed_us, result.outcome),
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
        let operation = Operation::new(trace_id, mount.to_string(), path.to_string(), op, mono_us);
        self.operations.insert(trace_id, operation);
        self.push_trace(trace_id);
        self.palette.color_for(mount);
        self.forest
            .mount_tree_mut(mount)
            .mark_in_flight(path, trace_id, mono_us);
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
        self.forest.mount_tree_mut(&mount).complete(
            &path,
            trace_id,
            elapsed_us,
            outcome == InspectorOutcome::Ok,
            mono_us,
        );
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
            operation.provider_id = Some(provider.to_string());
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

    fn on_provider_suspend(&mut self, trace_id: TraceId, callout_count: u32) {
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation.provider_suspended = true;
            operation.stages.push(Stage::done(
                StageKind::ProviderSuspend,
                format!("{callout_count} callout(s)"),
                None,
                None,
            ));
        }
    }

    fn on_provider_resume(&mut self, trace_id: TraceId, round: u32, result_count: u32) {
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation.provider_suspended = false;
            operation.stages.push(Stage::done(
                StageKind::ProviderResume,
                format!("round={round} results={result_count}"),
                None,
                None,
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
        if let Some(operation) = self.operations.get_mut(&trace_id) {
            operation.stages.push(Stage {
                kind: StageKind::Cache(kind),
                detail: scene::shorten_path(path, 48),
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
            self.forest.mount_tree_mut(mount).cache_hit(path, mono_us);
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
    use omnifs_inspector::OutcomeFields;

    fn record(mono_us: u64, event: InspectorEvent) -> InspectorRecord {
        InspectorRecord::new("2026-05-23T12:00:00Z", mono_us, event)
    }

    #[test]
    fn synthetic_trace_builds_fixed_waterfall() {
        let mut traces = TraceReducer::default();
        let events = [
            record(
                10,
                InspectorEvent::FuseStart {
                    trace_id: 7,
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: "/raulk/omnifs".into(),
                },
            ),
            record(
                20,
                InspectorEvent::ProviderStart {
                    trace_id: 7,
                    operation_id: 42,
                    mount: "github".into(),
                    provider: "github".into(),
                    method: "lookup_child".into(),
                    path: "/raulk/omnifs".into(),
                },
            ),
            record(
                30,
                InspectorEvent::CalloutStart {
                    trace_id: 7,
                    operation_id: 42,
                    callout_index: 0,
                    kind: CalloutKind::Fetch,
                    summary: "GET https://api.github.com/repos/raulk/omnifs".into(),
                },
            ),
            record(
                40,
                InspectorEvent::CalloutEnd {
                    trace_id: 7,
                    operation_id: 42,
                    callout_index: 0,
                    elapsed_us: 1_200,
                    result: OutcomeFields::ok(),
                },
            ),
            record(
                50,
                InspectorEvent::CacheEvent {
                    trace_id: 7,
                    operation_id: Some(42),
                    mount: "github".into(),
                    path: "/raulk/omnifs".into(),
                    kind: CacheKind::BrowseHit,
                    elapsed_us: Some(80),
                },
            ),
            record(
                60,
                InspectorEvent::ProviderEnd {
                    trace_id: 7,
                    operation_id: 42,
                    elapsed_us: 2_500,
                    result: OutcomeFields::ok(),
                },
            ),
            record(
                70,
                InspectorEvent::FuseEnd {
                    trace_id: 7,
                    op: "lookup".into(),
                    elapsed_us: 3_000,
                    result: OutcomeFields::ok(),
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
        assert_eq!(op.provider_id.as_deref(), Some("github"));
        assert!(op.stages.iter().all(|stage| !stage.in_flight));
    }

    #[test]
    fn trace_eviction_removes_operation_state() {
        let mut traces = TraceReducer::default();
        for trace_id in 0..(MAX_RECENT_TRACES as u64 + 2) {
            traces.apply_record(&record(
                trace_id,
                InspectorEvent::FuseStart {
                    trace_id,
                    op: "lookup".into(),
                    mount: "test".into(),
                    path: format!("/path/{trace_id}"),
                },
            ));
        }

        assert_eq!(traces.retained_trace_count(), MAX_RECENT_TRACES);
        assert_eq!(traces.operations.len(), MAX_RECENT_TRACES);
        assert!(traces.operation(0).is_none());
        assert!(traces.operation(1).is_none());
        assert!(
            traces
                .selected()
                .is_some_and(|trace_id| traces.operation(trace_id).is_some())
        );
    }
}
