//! Async callout correlation ID tracking.
//!
//! Assigns unique IDs to async callouts so providers can correlate
//! resume calls with pending operations.

use std::sync::atomic::{AtomicU64, Ordering};

pub struct CorrelationTracker {
    next: AtomicU64,
}

impl CorrelationTracker {
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    pub fn allocate(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for CorrelationTracker {
    fn default() -> Self {
        Self::new()
    }
}
