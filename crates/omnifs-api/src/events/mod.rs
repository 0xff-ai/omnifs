//! Shared JSONL schema for omnifs inspector observability.
//!
//! The host daemon emits [`InspectorRecord`] lines; the CLI `inspect` command reads them.

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
pub use wire::{ParseRecordError, split_complete_lines};
pub use writer::{InspectorLineWriter, LineWriteError};

/// FUSE-bound correlation id, one per FUSE request.
pub type TraceId = u64;
