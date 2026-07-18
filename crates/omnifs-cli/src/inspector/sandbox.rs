//! Per-mount sandbox port statistics.

use std::collections::{HashMap, HashSet, VecDeque};

use omnifs_api::events::{CalloutKind, InspectorOutcome, TraceId};

use super::metrics::MountWindow;

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
pub const UNTRACED_EXPORTS: [&str; 2] = ["initialize", "close_file"];
pub const IMPORT_PORTS: [CalloutKind; 3] = [
    CalloutKind::Fetch,
    CalloutKind::FetchBlob,
    CalloutKind::GitOpenRepo,
];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PortId {
    Export(String),
    Import(CalloutKind),
    Log,
}

pub fn export_port_ids(sandbox: Option<&MountSandboxView<'_>>) -> Vec<PortId> {
    let mut methods: Vec<String> = EXPORT_PORTS.iter().map(|&m| m.to_string()).collect();
    if let Some(sandbox) = sandbox {
        let mut extras: Vec<_> = sandbox.known_export_methods().map(str::to_string).collect();
        extras.sort();
        for method in extras {
            if !methods.iter().any(|m| m == &method) {
                methods.push(method);
            }
        }
    }
    methods.into_iter().map(PortId::Export).collect()
}

pub fn import_port_ids(sandbox: Option<&MountSandboxView<'_>>) -> Vec<PortId> {
    let mut kinds = IMPORT_PORTS.to_vec();
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

pub fn all_port_ids(sandbox: Option<&MountSandboxView<'_>>) -> Vec<PortId> {
    let mut ports = export_port_ids(sandbox);
    ports.extend(import_port_ids(sandbox));
    ports
}

#[derive(Debug, Default)]
pub struct PortStats {
    pub window: MountWindow,
    pub lifetime: u64,
}

#[derive(Debug, Clone)]
struct OpenCall {
    mount: String,
    port: PortId,
    summary: Option<String>,
    start_mono: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum OpenCallKey {
    Export {
        trace_id: TraceId,
        operation_id: u64,
    },
    Import {
        trace_id: TraceId,
        operation_id: u64,
        callout_index: u32,
    },
}

#[derive(Debug, Default)]
struct OpenCalls {
    entries: HashMap<OpenCallKey, OpenCall>,
}

impl OpenCalls {
    fn insert_export(&mut self, trace_id: TraceId, operation_id: u64, call: OpenCall) {
        self.entries.insert(
            OpenCallKey::Export {
                trace_id,
                operation_id,
            },
            call,
        );
    }

    fn insert_import(
        &mut self,
        trace_id: TraceId,
        operation_id: u64,
        callout_index: u32,
        call: OpenCall,
    ) {
        self.entries.insert(
            OpenCallKey::Import {
                trace_id,
                operation_id,
                callout_index,
            },
            call,
        );
    }

    fn remove_export(&mut self, trace_id: TraceId, operation_id: u64) -> Option<OpenCall> {
        self.entries.remove(&OpenCallKey::Export {
            trace_id,
            operation_id,
        })
    }

    fn remove_import(
        &mut self,
        trace_id: TraceId,
        operation_id: u64,
        callout_index: u32,
    ) -> Option<OpenCall> {
        self.entries.remove(&OpenCallKey::Import {
            trace_id,
            operation_id,
            callout_index,
        })
    }

    fn remove_trace(&mut self, trace_id: TraceId) {
        self.entries.retain(|key, _| match key {
            OpenCallKey::Export { trace_id: id, .. } | OpenCallKey::Import { trace_id: id, .. } => {
                *id != trace_id
            },
        });
    }
}

#[derive(Debug, Default)]
pub struct MountSandbox {
    pub provider: String,
    stats: HashMap<PortId, PortStats>,
    pub last_activity_mono: u64,
}

pub struct MountSandboxView<'a> {
    mount: &'a str,
    sandbox: &'a MountSandbox,
    open_calls: &'a OpenCalls,
}

pub struct OpenCallView<'a> {
    pub start_mono: u64,
    pub summary: Option<&'a str>,
}

impl<'a> MountSandboxView<'a> {
    fn calls(&self) -> impl Iterator<Item = &'a OpenCall> + '_ {
        self.open_calls
            .entries
            .values()
            .filter(move |call| call.mount == self.mount)
    }

    pub fn port_stats(&self, port: &PortId) -> Option<&PortStats> {
        self.sandbox.stats.get(port)
    }

    pub fn open_count(&self, port: &PortId) -> usize {
        self.calls().filter(|call| &call.port == port).count()
    }

    pub fn total_open_exports(&self) -> usize {
        self.calls()
            .filter(|call| matches!(call.port, PortId::Export(_)))
            .count()
    }

    pub fn total_open_imports(&self) -> usize {
        self.calls()
            .filter(|call| matches!(call.port, PortId::Import(_)))
            .count()
    }

    pub fn open_call(&self, port: &PortId) -> Option<OpenCallView<'_>> {
        self.calls()
            .find(|call| &call.port == port)
            .map(|call| OpenCallView {
                start_mono: call.start_mono,
                summary: call.summary.as_deref(),
            })
    }

    pub fn known_export_methods(&self) -> impl Iterator<Item = &str> + '_ {
        self.sandbox.stats.keys().filter_map(|port| match port {
            PortId::Export(method) => Some(method.as_str()),
            _ => None,
        })
    }

    pub fn known_import_kinds(&self) -> impl Iterator<Item = CalloutKind> + '_ {
        self.sandbox.stats.keys().filter_map(|port| match port {
            PortId::Import(kind) => Some(*kind),
            _ => None,
        })
    }

    pub fn provider(&self) -> &str {
        &self.sandbox.provider
    }
}

impl MountSandbox {
    fn note_activity(&mut self, mono_us: u64) {
        self.last_activity_mono = self.last_activity_mono.max(mono_us);
    }

    fn complete(
        &mut self,
        call: OpenCall,
        mono_us: u64,
        elapsed_us: u64,
        outcome: InspectorOutcome,
    ) {
        let stats = self.stats.entry(call.port).or_default();
        stats.window.record_completion(mono_us, elapsed_us, outcome);
        self.note_activity(mono_us);
    }
}

#[derive(Debug, Default)]
pub struct SandboxStats {
    mounts: HashMap<String, MountSandbox>,
    open_calls: OpenCalls,
    trace_order: VecDeque<TraceId>,
    seen_traces: HashSet<TraceId>,
}

impl SandboxStats {
    fn mount_mut(&mut self, mount: &str) -> &mut MountSandbox {
        self.mounts.entry(mount.to_string()).or_default()
    }

    pub fn mount_view(&self, mount: &str) -> Option<MountSandboxView<'_>> {
        let (mount, sandbox) = self.mounts.get_key_value(mount)?;
        Some(MountSandboxView {
            mount,
            sandbox,
            open_calls: &self.open_calls,
        })
    }

    pub fn mounts_by_activity(&self) -> Vec<&str> {
        let mut mounts: Vec<_> = self.mounts.iter().collect();
        mounts.sort_by_key(|(_, sandbox)| std::cmp::Reverse(sandbox.last_activity_mono));
        mounts
            .into_iter()
            .map(|(mount, _)| mount.as_str())
            .collect()
    }

    pub fn active_mount(&self, preferred: Option<&str>) -> Option<&str> {
        if let Some(preferred) = preferred
            && self.mounts.contains_key(preferred)
        {
            return self
                .mounts
                .get_key_value(preferred)
                .map(|(mount, _)| mount.as_str());
        }
        self.mounts
            .iter()
            .max_by_key(|(_, sandbox)| sandbox.last_activity_mono)
            .map(|(mount, _)| mount.as_str())
    }

    pub fn touch_trace(&mut self, trace_id: TraceId, cap: usize) {
        if self.seen_traces.insert(trace_id) {
            self.trace_order.push_back(trace_id);
        }
        while self.trace_order.len() > cap {
            if let Some(evicted) = self.trace_order.pop_front() {
                self.seen_traces.remove(&evicted);
                self.evict_trace(evicted);
            }
        }
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
        let port = PortId::Export(method.to_string());
        let mount_name = mount.to_string();
        {
            let sandbox = self.mount_mut(mount);
            sandbox.provider = provider.to_string();
            let stats = sandbox.stats.entry(port.clone()).or_default();
            stats.lifetime = stats.lifetime.saturating_add(1);
            sandbox.note_activity(mono_us);
        }
        self.open_calls.insert_export(
            trace_id,
            operation_id,
            OpenCall {
                mount: mount_name,
                port,
                summary: None,
                start_mono: mono_us,
            },
        );
    }

    pub fn on_provider_end(
        &mut self,
        trace_id: TraceId,
        operation_id: u64,
        elapsed_us: u64,
        outcome: InspectorOutcome,
        mono_us: u64,
    ) {
        if let Some(call) = self.open_calls.remove_export(trace_id, operation_id)
            && let Some(sandbox) = self.mounts.get_mut(&call.mount)
        {
            sandbox.complete(call, mono_us, elapsed_us, outcome);
        }
    }

    #[allow(clippy::too_many_arguments)]
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
        let port = PortId::Import(kind);
        let mount_name = mount.to_string();
        {
            let sandbox = self.mount_mut(mount);
            let stats = sandbox.stats.entry(port.clone()).or_default();
            stats.lifetime = stats.lifetime.saturating_add(1);
            sandbox.note_activity(mono_us);
        }
        self.open_calls.insert_import(
            trace_id,
            operation_id,
            callout_index,
            OpenCall {
                mount: mount_name,
                port,
                summary: Some(summary.to_string()),
                start_mono: mono_us,
            },
        );
    }

    pub fn on_callout_end(
        &mut self,
        trace_id: TraceId,
        operation_id: u64,
        callout_index: u32,
        elapsed_us: u64,
        outcome: InspectorOutcome,
        mono_us: u64,
    ) {
        if let Some(call) = self
            .open_calls
            .remove_import(trace_id, operation_id, callout_index)
            && let Some(sandbox) = self.mounts.get_mut(&call.mount)
        {
            sandbox.complete(call, mono_us, elapsed_us, outcome);
        }
    }

    pub fn evict_trace(&mut self, trace_id: TraceId) {
        self.open_calls.remove_trace(trace_id);
    }
}
