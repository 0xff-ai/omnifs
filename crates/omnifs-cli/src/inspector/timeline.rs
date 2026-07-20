//! Bounded ring of raw inspector records backing time-travel scrubbing.
//!
//! `TraceReducer` (in `trace_state.rs`) is a pure fold: it derives all of
//! its state from the sequence of `InspectorRecord`s applied to it and
//! carries no view state of its own. That means a scrubbed view of the
//! past is just a fresh `TraceReducer` folded over a prefix of the record
//! stream. `Timeline` is the record stream: a ring buffer addressed by
//! absolute ordinal (arrival order), so a scrub cursor can name a point
//! in history independent of how much the ring has since evicted.

use std::collections::VecDeque;

use omnifs_api::events::InspectorRecord;

/// Default ring capacity. Sized well above `MAX_RECENT_TRACES` operations
/// worth of raw events so a typical debugging session can scrub back
/// through everything the operations log still remembers, and then some.
pub const RING_CAP: usize = 65_536;

/// Bounded ring of every record that has arrived, oldest first.
pub struct Timeline {
    ring: VecDeque<InspectorRecord>,
    cap: usize,
    /// Absolute ordinal of `ring[0]`: how many records have been evicted
    /// from the front. This is the horizon a scrub cursor can fall behind.
    evicted: u64,
}

impl Timeline {
    pub fn new() -> Self {
        Self::with_capacity(RING_CAP)
    }

    /// Test/tuning constructor: a smaller cap exercises eviction without
    /// pushing tens of thousands of synthetic records.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            ring: VecDeque::new(),
            cap,
            evicted: 0,
        }
    }

    /// Append a record, evicting from the front once over capacity.
    pub fn push(&mut self, record: InspectorRecord) {
        self.ring.push_back(record);
        while self.ring.len() > self.cap {
            self.ring.pop_front();
            self.evicted += 1;
        }
    }

    /// Absolute ordinal one past the newest retained record.
    pub fn end(&self) -> u64 {
        self.evicted + self.ring.len() as u64
    }

    /// Absolute ordinal of the oldest retained record; the eviction horizon.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Look up a record by absolute ordinal. `None` if it has already
    /// been evicted or hasn't arrived yet.
    pub fn get(&self, ordinal: u64) -> Option<&InspectorRecord> {
        let offset = ordinal.checked_sub(self.evicted)?;
        self.ring.get(usize::try_from(offset).ok()?)
    }

    pub fn oldest_mono_us(&self) -> Option<u64> {
        self.ring.front().map(|record| record.mono_us)
    }

    /// Absolute ordinal of the first retained record whose `mono_us` is
    /// at or after `target`, clamped to `[evicted, end]`. Records are
    /// arrival-ordered and `mono_us` is monotone from one daemon, so a
    /// partition point over the ring is a valid binary search.
    pub fn ordinal_at_or_after(&self, target_mono_us: u64) -> u64 {
        let index = self
            .ring
            .partition_point(|record| record.mono_us < target_mono_us);
        self.evicted + index as u64
    }

    /// Clamp an absolute ordinal into the currently retained range.
    pub fn clamp_ordinal(&self, ordinal: u64) -> u64 {
        ordinal.clamp(self.evicted, self.end())
    }
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use omnifs_api::events::{InspectorEvent, InspectorRecord, TraceId};

    use super::*;

    fn record(trace_id: TraceId, mono_us: u64, event: InspectorEvent) -> InspectorRecord {
        InspectorRecord::new("2026-05-23T12:00:00Z", mono_us, trace_id, event)
    }

    fn synthetic_records() -> impl Iterator<Item = InspectorRecord> {
        (1_u64..=4).map(|trace_id| {
            record(
                trace_id,
                trace_id * 10,
                InspectorEvent::FuseStart {
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: format!("/{trace_id}"),
                },
            )
        })
    }

    #[test]
    fn push_and_get_round_trip_by_absolute_ordinal() {
        let mut timeline = Timeline::new();
        assert_eq!(timeline.end(), 0);
        assert_eq!(timeline.evicted(), 0);
        for record in synthetic_records() {
            timeline.push(record);
        }
        assert_eq!(timeline.end(), 4);
        assert_eq!(timeline.evicted(), 0);
        assert_eq!(timeline.get(0).map(|r| r.trace_id), Some(1));
        assert_eq!(timeline.get(3).map(|r| r.trace_id), Some(4));
        assert!(timeline.get(4).is_none());
    }

    #[test]
    fn eviction_advances_horizon_and_absolute_ordinals() {
        let mut timeline = Timeline::with_capacity(2);
        for record in synthetic_records() {
            timeline.push(record);
        }
        // Four pushes into a cap-2 ring: only the last 2 remain (end -
        // evicted == 2), and the horizon reports exactly how many were
        // evicted.
        assert_eq!(timeline.evicted(), 2);
        assert_eq!(timeline.end(), 4);
        assert!(timeline.get(1).is_none());
        assert!(timeline.get(2).is_some());
    }

    #[test]
    fn ordinal_at_or_after_finds_the_first_matching_mono_us() {
        let mut timeline = Timeline::new();
        for record in synthetic_records() {
            timeline.push(record);
        }
        let ordinal = timeline.ordinal_at_or_after(30);
        assert_eq!(timeline.get(ordinal).map(|r| r.mono_us), Some(30));
        // A target past the newest record clamps to `end`.
        assert_eq!(timeline.ordinal_at_or_after(u64::MAX), timeline.end());
    }

    #[test]
    fn horizon_clamp_never_panics_when_target_ordinal_is_evicted() {
        let mut timeline = Timeline::with_capacity(2);
        for record in synthetic_records() {
            timeline.push(record);
        }
        // Ordinal 0 was evicted long ago; clamping must land on the
        // horizon rather than underflow or panic.
        assert_eq!(timeline.clamp_ordinal(0), timeline.evicted());
        let clamped = timeline.clamp_ordinal(0);
        assert!(timeline.get(clamped).is_some() || clamped == timeline.end());
    }
}
