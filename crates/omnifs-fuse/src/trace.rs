//! FUSE operation trace logging.

use std::time::Instant;
use tracing::info;

pub(super) struct FuseTrace {
    op: &'static str,
    detail: String,
    start: Instant,
}

impl FuseTrace {
    pub(super) fn new(op: &'static str, detail: String) -> Self {
        Self {
            op,
            detail,
            start: Instant::now(),
        }
    }
}

impl Drop for FuseTrace {
    fn drop(&mut self) {
        info!(
            target: "omnifs_trace",
            kind = "fuse",
            op = self.op,
            detail = self.detail.as_str(),
            elapsed_us = self.start.elapsed().as_micros(),
            "trace_event"
        );
    }
}
