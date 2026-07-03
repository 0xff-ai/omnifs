use serde::{Deserialize, Serialize};

use crate::events::TraceId;
use crate::events::event::InspectorEvent;

pub const SCHEMA_VERSION: u32 = 1;

/// One JSONL inspector record.
///
/// `trace_id` lives on the envelope (not inside `event`) because every
/// inspector record belongs to a trace by definition; lifting it here
/// lets subscribers correlate across event types without matching the
/// variant first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectorRecord {
    pub v: u32,
    pub ts: String,
    pub mono_us: u64,
    /// Daemon-local emission sequence, monotonic across one daemon
    /// process. Used by subscribers to de-dup the small overlap window
    /// between a history snapshot and the inspector broadcast subscription.
    pub seq: u64,
    pub trace_id: TraceId,
    pub event: InspectorEvent,
}

impl InspectorRecord {
    pub fn new(
        ts: impl Into<String>,
        mono_us: u64,
        trace_id: TraceId,
        event: InspectorEvent,
    ) -> Self {
        Self {
            v: SCHEMA_VERSION,
            ts: ts.into(),
            mono_us,
            seq: 0,
            trace_id,
            event,
        }
    }

    #[must_use]
    pub fn with_seq(mut self, seq: u64) -> Self {
        self.seq = seq;
        self
    }
}
