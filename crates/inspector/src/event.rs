use serde::{Deserialize, Serialize};

use crate::TraceId;
use crate::kind::{CacheKind, CalloutKind};
use crate::outcome::OutcomeFields;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InspectorEvent {
    #[serde(rename = "fuse.start")]
    FuseStart {
        trace_id: TraceId,
        op: String,
        mount: String,
        path: String,
    },
    #[serde(rename = "fuse.end")]
    FuseEnd {
        trace_id: TraceId,
        op: String,
        elapsed_us: u64,
        #[serde(flatten)]
        result: OutcomeFields,
    },
    #[serde(rename = "provider.start")]
    ProviderStart {
        trace_id: TraceId,
        operation_id: u64,
        mount: String,
        provider: String,
        method: String,
        path: String,
    },
    #[serde(rename = "provider.suspend")]
    ProviderSuspend {
        trace_id: TraceId,
        operation_id: u64,
        callout_count: u32,
    },
    #[serde(rename = "provider.resume")]
    ProviderResume {
        trace_id: TraceId,
        operation_id: u64,
        round: u32,
        result_count: u32,
    },
    #[serde(rename = "callout.start")]
    CalloutStart {
        trace_id: TraceId,
        operation_id: u64,
        callout_index: u32,
        kind: CalloutKind,
        summary: String,
    },
    #[serde(rename = "callout.end")]
    CalloutEnd {
        trace_id: TraceId,
        operation_id: u64,
        callout_index: u32,
        elapsed_us: u64,
        #[serde(flatten)]
        result: OutcomeFields,
    },
    #[serde(rename = "subtree.start")]
    SubtreeStart {
        trace_id: TraceId,
        operation_id: u64,
        tree_ref: String,
    },
    #[serde(rename = "subtree.end")]
    SubtreeEnd {
        trace_id: TraceId,
        operation_id: u64,
        tree_ref: String,
        elapsed_us: u64,
        #[serde(flatten)]
        result: OutcomeFields,
    },
    #[serde(rename = "clone.start")]
    CloneStart {
        trace_id: TraceId,
        operation_id: u64,
        cache_key: String,
        remote: String,
    },
    #[serde(rename = "clone.end")]
    CloneEnd {
        trace_id: TraceId,
        operation_id: u64,
        cache_key: String,
        elapsed_us: u64,
        #[serde(flatten)]
        result: OutcomeFields,
    },
    #[serde(rename = "cache.event")]
    CacheEvent {
        trace_id: TraceId,
        #[serde(skip_serializing_if = "Option::is_none")]
        operation_id: Option<u64>,
        mount: String,
        path: String,
        kind: CacheKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_us: Option<u64>,
    },
    #[serde(rename = "provider.end")]
    ProviderEnd {
        trace_id: TraceId,
        operation_id: u64,
        elapsed_us: u64,
        #[serde(flatten)]
        result: OutcomeFields,
    },
}
