//! Shared JSONL schema for omnifs inspector observability.
//!
//! Every Inspector stream uses [`InspectorLine`] as its newline unit. The
//! typed line owns JSONL framing and validates the nested record schema.

#![forbid(unsafe_code)]

mod envelope;
mod event;
mod kind;
mod outcome;
mod redaction;
mod wire;
mod writer;

pub use envelope::{InspectorRecord, SCHEMA_VERSION};
pub use event::{InspectorEvent, OpEnd};
pub use kind::{CacheKind, CalloutKind};
pub use outcome::{InspectorOutcome, OutcomeFields};
pub use redaction::{
    is_sensitive_header, is_sensitive_query_param, redact_git_remote, redact_http_url_for_summary,
    write_truncated,
};
pub use wire::{ParseLineError, split_complete_lines};
pub use writer::{InspectorLineWriter, LineWriteError};

/// Typed lines sent after a control-plane inspector subscription is ready.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum InspectorLine {
    Record(InspectorRecord),
    Dropped { count: u64 },
}

/// FUSE-bound correlation id, one per FUSE request.
pub type TraceId = u64;
