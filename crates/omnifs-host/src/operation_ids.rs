//! Operation ID allocation.
//!
//! Assigns unique monotonic IDs to provider operations so that callout
//! resume calls can be correlated with the originating operation.

use std::sync::atomic::{AtomicU64, Ordering};

pub struct OperationIds {
    next: AtomicU64,
}

impl OperationIds {
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    pub fn allocate(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for OperationIds {
    fn default() -> Self {
        Self::new()
    }
}
