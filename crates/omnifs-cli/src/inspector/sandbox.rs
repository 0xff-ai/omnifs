//! Per-mount sandbox port statistics.

use std::collections::HashMap;

use omnifs_api::events::{CalloutKind, InspectorOutcome, TraceId};

use super::metrics::MountWindow;

const MAX_OPEN_CALLS: usize = 8_192;

const EXPORT_PORTS: [&str; 8] = [
    "initialize",
    "lookup_child",
    "list_children",
    "read_file",
    "open_file",
    "read_chunk",
    "close_file",
    "on_event",
];
const UNTRACED_EXPORTS: [&str; 2] = ["initialize", "close_file"];
const IMPORT_PORTS: [CalloutKind; 3] = [
    CalloutKind::Fetch,
    CalloutKind::FetchBlob,
    CalloutKind::GitOpenRepo,
];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PortId {
    Export(String),
    Import(CalloutKind),
}

impl PortId {
    pub fn label(&self) -> String {
        match self {
            Self::Export(method) => method.replace('_', "-"),
            Self::Import(kind) => kind.as_str().replace('_', "-"),
        }
    }

    pub fn is_untraced(&self) -> bool {
        matches!(self, Self::Export(method) if UNTRACED_EXPORTS.contains(&method.as_str()))
    }

    pub const fn is_export(&self) -> bool {
        matches!(self, Self::Export(_))
    }

    pub fn exports() -> Vec<Self> {
        EXPORT_PORTS
            .iter()
            .map(|method| Self::Export((*method).to_string()))
            .collect()
    }

    pub fn imports() -> Vec<Self> {
        IMPORT_PORTS.iter().copied().map(Self::Import).collect()
    }

    pub fn all() -> Vec<Self> {
        let mut ports = Self::exports();
        ports.extend(Self::imports());
        ports
    }
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

#[derive(Debug, Default)]
pub struct MountSandbox {
    provider: String,
    stats: HashMap<PortId, PortStats>,
    last_activity_mono: u64,
}

pub struct MountSandboxView<'a> {
    mount: &'a str,
    sandbox: &'a MountSandbox,
    open_calls: &'a HashMap<(TraceId, u64, Option<u32>), OpenCall>,
}

impl<'a> MountSandboxView<'a> {
    fn calls(&self) -> impl Iterator<Item = &'a OpenCall> + '_ {
        self.open_calls
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

    pub fn open_call(&self, port: &PortId) -> Option<(u64, Option<&str>)> {
        self.calls()
            .find(|call| &call.port == port)
            .map(|call| (call.start_mono, call.summary.as_deref()))
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
    open_calls: HashMap<(TraceId, u64, Option<u32>), OpenCall>,
}

impl SandboxStats {
    fn insert_open_call(&mut self, key: (TraceId, u64, Option<u32>), call: OpenCall) {
        self.open_calls.insert(key, call);
        if self.open_calls.len() > MAX_OPEN_CALLS
            && let Some(key) = self
                .open_calls
                .iter()
                .min_by_key(|(_, call)| call.start_mono)
                .map(|(key, _)| *key)
        {
            self.open_calls.remove(&key);
        }
    }

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
        self.insert_open_call(
            (trace_id, operation_id, None),
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
        if let Some(call) = self.open_calls.remove(&(trace_id, operation_id, None))
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
        self.insert_open_call(
            (trace_id, operation_id, Some(callout_index)),
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
            .remove(&(trace_id, operation_id, Some(callout_index)))
            && let Some(sandbox) = self.mounts.get_mut(&call.mount)
        {
            sandbox.complete(call, mono_us, elapsed_us, outcome);
        }
    }

    pub fn remove_trace(&mut self, trace_id: TraceId) {
        self.open_calls.retain(|(id, _, _), _| *id != trace_id);
    }
}
