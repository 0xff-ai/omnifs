use serde::{Deserialize, Serialize};

use crate::event::InspectorEvent;

pub const SCHEMA_VERSION: u32 = 1;

/// One JSONL inspector record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectorRecord {
    pub v: u32,
    pub ts: String,
    pub mono_us: u64,
    /// Daemon-local emission sequence, monotonic across one daemon
    /// process. Used by subscribers to de-dup the small overlap window
    /// between a history snapshot and the inspector broadcast subscription.
    /// Defaults to 0 for compatibility with v1 records written before
    /// this field existed.
    #[serde(default)]
    pub seq: u64,
    pub event: InspectorEvent,
}

impl InspectorRecord {
    pub fn new(ts: impl Into<String>, mono_us: u64, event: InspectorEvent) -> Self {
        Self {
            v: SCHEMA_VERSION,
            ts: ts.into(),
            mono_us,
            seq: 0,
            event,
        }
    }

    #[must_use]
    pub fn with_seq(mut self, seq: u64) -> Self {
        self.seq = seq;
        self
    }
}
