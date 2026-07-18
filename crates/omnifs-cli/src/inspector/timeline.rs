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
    use omnifs_api::events::{
        CacheKind, CalloutKind, InspectorEvent, InspectorOutcome, InspectorRecord, OpEnd,
        OutcomeFields, TraceId,
    };

    use super::*;
    use crate::inspector::filter::ViewFilter;
    use crate::inspector::sandbox::MountSandbox;
    use crate::inspector::trace_state::TraceReducer;

    fn record(trace_id: TraceId, mono_us: u64, event: InspectorEvent) -> InspectorRecord {
        InspectorRecord::new("2026-05-23T12:00:00Z", mono_us, trace_id, event)
    }

    /// ~20 records across 3 traces covering fuse, provider, callout,
    /// cache, subtree, and clone events, arrival-ordered with monotone
    /// `mono_us` as a single daemon would emit them. Long because it's
    /// flat literal test data, not control flow.
    #[allow(clippy::too_many_lines)]
    fn synthetic_records() -> Vec<InspectorRecord> {
        let mut mono = 0_u64;
        let mut next = |event: InspectorEvent, trace_id: TraceId| -> InspectorRecord {
            mono += 10;
            record(trace_id, mono, event)
        };

        vec![
            next(
                InspectorEvent::FuseStart {
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: "/raulk/omnifs".into(),
                },
                1,
            ),
            next(
                InspectorEvent::FuseStart {
                    op: "write".into(),
                    mount: "scratch".into(),
                    path: "/notes".into(),
                },
                2,
            ),
            next(
                InspectorEvent::ProviderStart {
                    operation_id: 100,
                    mount: "github".into(),
                    provider: "github".into(),
                    method: "lookup_child".into(),
                    path: "/raulk/omnifs".into(),
                },
                1,
            ),
            next(
                InspectorEvent::ProviderStart {
                    operation_id: 200,
                    mount: "scratch".into(),
                    provider: "scratch".into(),
                    method: "write_file".into(),
                    path: "/notes".into(),
                },
                2,
            ),
            next(
                InspectorEvent::CalloutStart {
                    operation_id: 100,
                    callout_index: 0,
                    kind: CalloutKind::Fetch,
                    summary: "GET api.github.com/repos/raulk/omnifs".into(),
                },
                1,
            ),
            next(
                InspectorEvent::CalloutStart {
                    operation_id: 200,
                    callout_index: 0,
                    kind: CalloutKind::FetchBlob,
                    summary: "PUT blob".into(),
                },
                2,
            ),
            next(
                InspectorEvent::CalloutEnd {
                    operation_id: 100,
                    callout_index: 0,
                    end: OpEnd {
                        elapsed_us: 1_200,
                        result: OutcomeFields::ok(),
                    },
                },
                1,
            ),
            next(
                InspectorEvent::CalloutEnd {
                    operation_id: 200,
                    callout_index: 0,
                    end: OpEnd {
                        elapsed_us: 900,
                        result: OutcomeFields::ok(),
                    },
                },
                2,
            ),
            next(
                InspectorEvent::CacheEvent {
                    operation_id: Some(100),
                    mount: "github".into(),
                    path: "/raulk/omnifs".into(),
                    kind: CacheKind::BrowseHit,
                    elapsed_us: None,
                },
                1,
            ),
            next(
                InspectorEvent::CacheEvent {
                    operation_id: Some(200),
                    mount: "scratch".into(),
                    path: "/notes".into(),
                    kind: CacheKind::FileMiss,
                    elapsed_us: None,
                },
                2,
            ),
            next(
                InspectorEvent::ProviderEnd {
                    operation_id: 100,
                    end: OpEnd {
                        elapsed_us: 2_500,
                        result: OutcomeFields::ok(),
                    },
                },
                1,
            ),
            next(
                InspectorEvent::ProviderEnd {
                    operation_id: 200,
                    end: OpEnd {
                        elapsed_us: 1_800,
                        result: OutcomeFields::ok(),
                    },
                },
                2,
            ),
            next(
                InspectorEvent::FuseEnd {
                    op: "lookup".into(),
                    end: OpEnd {
                        elapsed_us: 3_000,
                        result: OutcomeFields::ok(),
                    },
                },
                1,
            ),
            next(
                InspectorEvent::FuseEnd {
                    op: "write".into(),
                    end: OpEnd {
                        elapsed_us: 2_200,
                        result: OutcomeFields::ok(),
                    },
                },
                2,
            ),
            next(
                InspectorEvent::FuseStart {
                    op: "lookup".into(),
                    mount: "github".into(),
                    path: "/raulk/omnifs2".into(),
                },
                3,
            ),
            next(
                InspectorEvent::SubtreeStart {
                    operation_id: 300,
                    tree_ref: "sub-1".into(),
                },
                3,
            ),
            next(
                InspectorEvent::SubtreeEnd {
                    operation_id: 300,
                    tree_ref: "sub-1".into(),
                    end: OpEnd {
                        elapsed_us: 400,
                        result: OutcomeFields::ok(),
                    },
                },
                3,
            ),
            next(
                InspectorEvent::CloneStart {
                    operation_id: 300,
                    cache_key: "abc123".into(),
                    remote: "https://github.com/raulk/omnifs".into(),
                },
                3,
            ),
            next(
                InspectorEvent::CloneEnd {
                    operation_id: 300,
                    cache_key: "abc123".into(),
                    end: OpEnd {
                        elapsed_us: 5_000,
                        result: OutcomeFields::ok(),
                    },
                },
                3,
            ),
            next(
                InspectorEvent::FuseEnd {
                    op: "lookup".into(),
                    end: OpEnd {
                        elapsed_us: 6_000,
                        result: OutcomeFields::with_outcome(InspectorOutcome::NotFound),
                    },
                },
                3,
            ),
        ]
    }

    #[test]
    fn push_and_get_round_trip_by_absolute_ordinal() {
        let mut timeline = Timeline::new();
        assert_eq!(timeline.end(), 0);
        assert_eq!(timeline.evicted(), 0);
        for record in synthetic_records() {
            timeline.push(record);
        }
        assert_eq!(timeline.end(), 20);
        assert_eq!(timeline.evicted(), 0);
        assert_eq!(timeline.get(0).map(|r| r.trace_id), Some(1));
        assert_eq!(timeline.get(19).map(|r| r.trace_id), Some(3));
        assert!(timeline.get(20).is_none());
    }

    #[test]
    fn eviction_advances_horizon_and_absolute_ordinals() {
        let mut timeline = Timeline::with_capacity(4);
        for record in synthetic_records() {
            timeline.push(record);
        }
        // 20 pushes into a cap-4 ring: only the last 4 remain (end -
        // evicted == 4), and the horizon reports exactly how many were
        // evicted.
        assert_eq!(timeline.evicted(), 16);
        assert_eq!(timeline.end(), 20);
        assert!(timeline.get(15).is_none());
        assert!(timeline.get(16).is_some());
    }

    #[test]
    fn ordinal_at_or_after_finds_the_first_matching_mono_us() {
        let mut timeline = Timeline::new();
        for record in synthetic_records() {
            timeline.push(record);
        }
        // Ordinal 4 (0-based) carries mono_us 50 in the synthetic stream.
        let ordinal = timeline.ordinal_at_or_after(50);
        assert_eq!(timeline.get(ordinal).map(|r| r.mono_us), Some(50));
        // A target past the newest record clamps to `end`.
        assert_eq!(timeline.ordinal_at_or_after(u64::MAX), timeline.end());
    }

    /// Determinism: folding the whole stream at once must land on the
    /// same observable projections as folding a prefix, then folding the
    /// rest. This is the property the scrub cursor's incremental step and
    /// full-rebuild code paths both rely on being equivalent.
    #[test]
    fn refolding_a_prefix_then_the_rest_matches_folding_all_at_once() {
        let mut timeline = Timeline::new();
        for record in synthetic_records() {
            timeline.push(record);
        }

        let mut full = TraceReducer::default();
        for ordinal in 0..timeline.end() {
            full.apply_record(timeline.get(ordinal).expect("retained"));
        }

        let split = timeline.end() / 2;
        let mut incremental = TraceReducer::default();
        for ordinal in 0..split {
            incremental.apply_record(timeline.get(ordinal).expect("retained"));
        }
        for ordinal in split..timeline.end() {
            incremental.apply_record(timeline.get(ordinal).expect("retained"));
        }

        let filter = ViewFilter::default();
        assert_eq!(
            full.visible_trace_ids(&filter),
            incremental.visible_trace_ids(&filter)
        );
        assert_eq!(
            full.retained_trace_count(),
            incremental.retained_trace_count()
        );

        let labels = |reducer: &TraceReducer| -> Vec<String> {
            reducer
                .operation(1)
                .expect("trace 1")
                .stages
                .iter()
                .map(|stage| stage.kind.display_label().into_owned())
                .collect()
        };
        assert_eq!(labels(&full), labels(&incremental));

        // Same determinism property, projected through the sandbox fold:
        // trace 1's provider.start/end pair on github's lookup_child
        // export completes fully in the synthetic stream, so both the
        // lifetime counter and the open-call count must agree between
        // the whole-stream fold and the split-then-resumed fold.
        let export_lifetime = |reducer: &TraceReducer| -> u64 {
            reducer
                .mount_sandbox("github")
                .map_or(0, |sandbox| sandbox.export_lifetime_count("lookup_child"))
        };
        assert_eq!(export_lifetime(&full), export_lifetime(&incremental));

        let open_exports = |reducer: &TraceReducer| -> usize {
            reducer
                .mount_sandbox("github")
                .map_or(0, MountSandbox::total_open_exports)
        };
        assert_eq!(open_exports(&full), open_exports(&incremental));
    }

    #[test]
    fn horizon_clamp_never_panics_when_target_ordinal_is_evicted() {
        let mut timeline = Timeline::with_capacity(4);
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
