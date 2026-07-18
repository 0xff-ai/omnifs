//! Per-mount sandbox port statistics.
//!
//! Every provider runs in a Wasm sandbox with a fixed export surface
//! (the methods the host calls into) and a small import surface (the
//! host callouts the provider awaits). This module folds the same
//! `ProviderStart`/`ProviderEnd`/`CalloutStart`/`CalloutEnd` events
//! `TraceReducer` already consumes into a per-mount, per-port view: a
//! sliding-window rate/latency profile plus in-flight open-call counts
//! for each port. It feeds the sandbox map view (exports on the left,
//! imports on the right of the mount's sandbox rectangle).
//!
//! Like the rest of the fold, this state is rebuilt purely from
//! `InspectorRecord`s; it reads no wall clock.

use std::collections::HashMap;

use omnifs_api::events::{CalloutKind, InspectorOutcome, TraceId};

use super::metrics::MountWindow;

/// The eight WIT-exported provider methods, in the fixed order the
/// sandbox map renders them (left column, top to bottom).
pub const EXPORT_PORTS: [&str; 8] = [
    "initialize",
    "lookup_child",
    "list_children",
    "read_file",
    "open_file",
    "read_chunk",
    "close_file",
    "on_event",
];

/// Exports that never carry a `provider.start`/`provider.end` pair in
/// the trace stream (component lifecycle hooks, not per-request
/// dispatch). The map renders these dim rather than implying a port
/// that's simply untraced is a port that's never called.
pub const UNTRACED_EXPORTS: [&str; 2] = ["initialize", "close_file"];

/// The three WIT-imported callout kinds, in the fixed order the
/// sandbox map renders them (right column, top to bottom) before the
/// always-last Log pseudo-port.
pub const IMPORT_PORTS: [CalloutKind; 3] = [
    CalloutKind::Fetch,
    CalloutKind::FetchBlob,
    CalloutKind::GitOpenRepo,
];

/// One row in the sandbox map's port rails: an exported method, an
/// imported callout kind, or the always-last Log pseudo-port. `Log`
/// carries no port stats (provider log lines never open a start/end
/// pair) so the map renders it as a permanently untraced label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortId {
    Export(String),
    Import(CalloutKind),
    Log,
}

/// Export rows for one mount's rail: [`EXPORT_PORTS`] in its fixed
/// order, then any method the provider actually called that isn't in
/// that static list. The static list is the known WIT surface; the
/// fallback exists so a newly added export method still shows up on
/// the map instead of silently vanishing when the SDK grows one.
pub fn export_port_ids(sandbox: Option<&MountSandbox>) -> Vec<PortId> {
    let mut methods: Vec<String> = EXPORT_PORTS.iter().map(|&m| m.to_string()).collect();
    if let Some(sandbox) = sandbox {
        for method in sandbox.known_export_methods() {
            if !methods.iter().any(|m| m == method) {
                methods.push(method.to_string());
            }
        }
    }
    methods.into_iter().map(PortId::Export).collect()
}

/// Import rows for one mount's rail: [`IMPORT_PORTS`] in its fixed
/// order, then any kind the provider actually fired that isn't in that
/// static list, then the always-last Log pseudo-port.
pub fn import_port_ids(sandbox: Option<&MountSandbox>) -> Vec<PortId> {
    let mut kinds: Vec<CalloutKind> = IMPORT_PORTS.to_vec();
    if let Some(sandbox) = sandbox {
        for kind in sandbox.known_import_kinds() {
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
    }
    let mut ports: Vec<PortId> = kinds.into_iter().map(PortId::Import).collect();
    ports.push(PortId::Log);
    ports
}

/// The full cursor-navigable port list for one mount: every export
/// row top to bottom, then every import row top to bottom (`Log`
/// included, always last).
pub fn all_port_ids(sandbox: Option<&MountSandbox>) -> Vec<PortId> {
    let mut ports = export_port_ids(sandbox);
    ports.extend(import_port_ids(sandbox));
    ports
}

/// One in-flight exported-method call, keyed by `(trace_id,
/// operation_id)` in [`MountSandbox::open_exports`].
#[derive(Debug, Clone)]
struct OpenExport {
    method: String,
    start_mono: u64,
}

/// One in-flight imported callout, keyed by `(trace_id, operation_id,
/// callout_index)` in [`MountSandbox::open_imports`]. `summary` is
/// retained (not just the kind) so the map's pinned-port detail panel
/// can show the actual request, e.g. the URL a `fetch` call is waiting
/// on.
#[derive(Debug, Clone)]
struct OpenImport {
    kind: CalloutKind,
    summary: String,
    start_mono: u64,
}

/// Sandbox port stats for one mount: a window and lifetime counter per
/// exported method, a window and lifetime counter per imported callout
/// kind, and open-call tracking for both.
#[derive(Debug, Default)]
pub struct MountSandbox {
    pub provider: String,
    pub exports: HashMap<String, MountWindow>,
    pub imports: HashMap<CalloutKind, MountWindow>,
    open_exports: HashMap<(TraceId, u64), OpenExport>,
    open_imports: HashMap<(TraceId, u64, u32), OpenImport>,
    /// Total invocations per export port across the whole session.
    /// `MountWindow` only covers the trailing 60s, so a port that's
    /// gone quiet still needs a lifetime total to distinguish "never
    /// called" from "called earlier, quiet now" on the map.
    export_lifetime: HashMap<String, u64>,
    import_lifetime: HashMap<CalloutKind, u64>,
    pub last_activity_mono: u64,
}

impl MountSandbox {
    /// The 60s sliding window for one export port, if it's ever been called.
    pub fn export_window(&self, method: &str) -> Option<&MountWindow> {
        self.exports.get(method)
    }

    /// The 60s sliding window for one import kind, if it's ever fired.
    pub fn import_window(&self, kind: CalloutKind) -> Option<&MountWindow> {
        self.imports.get(&kind)
    }

    /// How many calls to this export method are currently in flight.
    pub fn export_open_count(&self, method: &str) -> usize {
        self.open_exports
            .values()
            .filter(|open| open.method == method)
            .count()
    }

    /// How many callouts of this kind are currently in flight.
    pub fn import_open_count(&self, kind: CalloutKind) -> usize {
        self.open_imports
            .values()
            .filter(|open| open.kind == kind)
            .count()
    }

    /// Total in-flight export calls across every method.
    pub fn total_open_exports(&self) -> usize {
        self.open_exports.len()
    }

    /// Total in-flight callouts across every kind.
    pub fn total_open_imports(&self) -> usize {
        self.open_imports.len()
    }

    /// Total invocations of this export method across the whole
    /// session, not just the trailing 60s.
    pub fn export_lifetime_count(&self, method: &str) -> u64 {
        self.export_lifetime.get(method).copied().unwrap_or(0)
    }

    /// Total invocations of this import kind across the whole session.
    pub fn import_lifetime_count(&self, kind: CalloutKind) -> u64 {
        self.import_lifetime.get(&kind).copied().unwrap_or(0)
    }

    /// Export methods that have seen at least one call. Ports with no
    /// traffic yet are absent rather than zeroed, so the map can tell
    /// "never called" apart from "called, currently idle".
    pub fn known_export_methods(&self) -> impl Iterator<Item = &str> + '_ {
        self.exports.keys().map(String::as_str)
    }

    /// Import kinds that have seen at least one callout.
    pub fn known_import_kinds(&self) -> impl Iterator<Item = CalloutKind> + '_ {
        self.imports.keys().copied()
    }

    /// In-flight export calls, as (method, started-at-mono) pairs, for
    /// the map's pinned-port detail to render an elapsed duration.
    pub fn open_export_calls(&self) -> impl Iterator<Item = (&str, u64)> + '_ {
        self.open_exports
            .values()
            .map(|open| (open.method.as_str(), open.start_mono))
    }

    /// In-flight callouts, as (kind, summary, started-at-mono) triples.
    pub fn open_import_calls(&self) -> impl Iterator<Item = (CalloutKind, &str, u64)> + '_ {
        self.open_imports
            .values()
            .map(|open| (open.kind, open.summary.as_str(), open.start_mono))
    }

    fn note_activity(&mut self, mono_us: u64) {
        self.last_activity_mono = self.last_activity_mono.max(mono_us);
    }
}

/// Per-mount sandbox stats, keyed by mount name.
#[derive(Debug, Default)]
pub struct SandboxStats {
    mounts: HashMap<String, MountSandbox>,
}

impl SandboxStats {
    fn mount_mut(&mut self, mount: &str) -> &mut MountSandbox {
        self.mounts.entry(mount.to_string()).or_default()
    }

    pub fn mount(&self, mount: &str) -> Option<&MountSandbox> {
        self.mounts.get(mount)
    }

    /// Mounts with any sandbox activity, most recent first.
    pub fn mounts_by_activity(&self) -> Vec<&str> {
        let mut mounts: Vec<_> = self.mounts.iter().collect();
        mounts.sort_by_key(|(_, sandbox)| std::cmp::Reverse(sandbox.last_activity_mono));
        mounts
            .into_iter()
            .map(|(mount, _)| mount.as_str())
            .collect()
    }

    pub fn on_provider_start(
        &mut self,
        mount: &str,
        trace_id: TraceId,
        operation_id: u64,
        provider: &str,
        method: &str,
        mono_us: u64,
    ) {
        let sandbox = self.mount_mut(mount);
        sandbox.provider = provider.to_string();
        sandbox.open_exports.insert(
            (trace_id, operation_id),
            OpenExport {
                method: method.to_string(),
                start_mono: mono_us,
            },
        );
        *sandbox
            .export_lifetime
            .entry(method.to_string())
            .or_insert(0) += 1;
        sandbox.note_activity(mono_us);
    }

    pub fn on_provider_end(
        &mut self,
        mount: &str,
        trace_id: TraceId,
        operation_id: u64,
        elapsed_us: u64,
        outcome: InspectorOutcome,
        mono_us: u64,
    ) {
        let sandbox = self.mount_mut(mount);
        let Some(open) = sandbox.open_exports.remove(&(trace_id, operation_id)) else {
            return;
        };
        sandbox
            .exports
            .entry(open.method)
            .or_default()
            .record_completion(mono_us, elapsed_us, outcome);
        sandbox.note_activity(mono_us);
    }

    pub fn on_callout_start(
        &mut self,
        mount: &str,
        trace_id: TraceId,
        operation_id: u64,
        callout_index: u32,
        kind: CalloutKind,
        summary: &str,
        mono_us: u64,
    ) {
        let sandbox = self.mount_mut(mount);
        sandbox.open_imports.insert(
            (trace_id, operation_id, callout_index),
            OpenImport {
                kind,
                summary: summary.to_string(),
                start_mono: mono_us,
            },
        );
        *sandbox.import_lifetime.entry(kind).or_insert(0) += 1;
        sandbox.note_activity(mono_us);
    }

    pub fn on_callout_end(
        &mut self,
        mount: &str,
        trace_id: TraceId,
        operation_id: u64,
        callout_index: u32,
        elapsed_us: u64,
        outcome: InspectorOutcome,
        mono_us: u64,
    ) {
        let sandbox = self.mount_mut(mount);
        let Some(open) = sandbox
            .open_imports
            .remove(&(trace_id, operation_id, callout_index))
        else {
            return;
        };
        sandbox
            .imports
            .entry(open.kind)
            .or_default()
            .record_completion(mono_us, elapsed_us, outcome);
        sandbox.note_activity(mono_us);
    }

    /// Sweep every open-call entry belonging to an evicted trace, across
    /// every mount. Without this, a trace evicted mid-flight (its
    /// `provider.start` seen, its `provider.end` never arriving before
    /// eviction) would leak an open_exports/open_imports entry forever.
    pub fn evict_trace(&mut self, trace_id: TraceId) {
        for sandbox in self.mounts.values_mut() {
            sandbox.open_exports.retain(|(tid, _), _| *tid != trace_id);
            sandbox
                .open_imports
                .retain(|(tid, _, _), _| *tid != trace_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_ports_follow_the_fixed_order_with_drift_appended() {
        let mut stats = SandboxStats::default();
        // A method outside the static EXPORT_PORTS list: SDK drift, an
        // export the map has never seen a name for before. Completing
        // the call (not just starting it) is what makes it "known":
        // `known_export_methods` reads the completed-call window map,
        // populated by `on_provider_end`.
        stats.on_provider_start("github", 1, 1, "github", "custom_method", 10);
        stats.on_provider_end("github", 1, 1, 5, InspectorOutcome::Ok, 20);
        let ports = export_port_ids(stats.mount("github"));
        let names: Vec<&str> = ports
            .iter()
            .map(|p| match p {
                PortId::Export(m) => m.as_str(),
                _ => unreachable!("export_port_ids only ever yields Export rows"),
            })
            .collect();
        assert_eq!(&names[..EXPORT_PORTS.len()], &EXPORT_PORTS);
        assert_eq!(names.last(), Some(&"custom_method"));
    }

    #[test]
    fn log_pseudo_port_is_always_last_in_the_import_list() {
        let ports = import_port_ids(None);
        assert_eq!(ports.last(), Some(&PortId::Log));
        // Base kinds keep their fixed order ahead of Log.
        assert_eq!(
            ports[..IMPORT_PORTS.len()],
            [
                PortId::Import(CalloutKind::Fetch),
                PortId::Import(CalloutKind::FetchBlob),
                PortId::Import(CalloutKind::GitOpenRepo),
            ]
        );
    }

    #[test]
    fn all_port_ids_is_exports_then_imports() {
        let ports = all_port_ids(None);
        assert_eq!(ports.len(), EXPORT_PORTS.len() + IMPORT_PORTS.len() + 1);
        assert_eq!(ports[0], PortId::Export(EXPORT_PORTS[0].to_string()));
        assert_eq!(ports.last(), Some(&PortId::Log));
    }
}
