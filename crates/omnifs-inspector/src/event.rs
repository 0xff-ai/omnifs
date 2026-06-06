use serde::{Deserialize, Serialize};

use crate::kind::{CacheKind, CalloutKind};
use crate::outcome::OutcomeFields;

/// Shared tail of every `*End` variant: elapsed wall time plus outcome.
/// `OutcomeFields` is itself flattened on the wire so `outcome` and the
/// optional `message` land alongside `elapsed_us`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpEnd {
    pub elapsed_us: u64,
    #[serde(flatten)]
    pub result: OutcomeFields,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InspectorEvent {
    #[serde(rename = "fuse.start")]
    FuseStart {
        op: String,
        mount: String,
        path: String,
    },
    #[serde(rename = "fuse.end")]
    FuseEnd {
        op: String,
        #[serde(flatten)]
        end: OpEnd,
    },
    #[serde(rename = "provider.start")]
    ProviderStart {
        operation_id: u64,
        mount: String,
        provider: String,
        method: String,
        path: String,
    },
    #[serde(rename = "provider.suspend")]
    ProviderSuspend {
        operation_id: u64,
        callout_count: u32,
    },
    #[serde(rename = "provider.resume")]
    ProviderResume {
        operation_id: u64,
        round: u32,
        result_count: u32,
    },
    #[serde(rename = "callout.start")]
    CalloutStart {
        operation_id: u64,
        callout_index: u32,
        kind: CalloutKind,
        summary: String,
    },
    #[serde(rename = "callout.end")]
    CalloutEnd {
        operation_id: u64,
        callout_index: u32,
        #[serde(flatten)]
        end: OpEnd,
    },
    #[serde(rename = "subtree.start")]
    SubtreeStart { operation_id: u64, tree_ref: String },
    #[serde(rename = "subtree.end")]
    SubtreeEnd {
        operation_id: u64,
        tree_ref: String,
        #[serde(flatten)]
        end: OpEnd,
    },
    #[serde(rename = "clone.start")]
    CloneStart {
        operation_id: u64,
        cache_key: String,
        remote: String,
    },
    #[serde(rename = "clone.end")]
    CloneEnd {
        operation_id: u64,
        cache_key: String,
        #[serde(flatten)]
        end: OpEnd,
    },
    #[serde(rename = "cache.event")]
    CacheEvent {
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
        operation_id: u64,
        #[serde(flatten)]
        end: OpEnd,
    },
}
