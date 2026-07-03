//! Sliding-window metrics per mount: event rate, error rate, cache hit
//! ratio, p95 latency, and a sparkline byte-strip.
//!
//! Each `MountWindow` keeps the last `WINDOW_US` worth of completion
//! samples; older samples are dropped lazily on each insert.

use std::collections::VecDeque;

use omnifs_api::events::InspectorOutcome;

const WINDOW_US: u64 = 60_000_000; // 60s
const MAX_SAMPLES: usize = 4096;
/// 8 levels mapped to ▁▂▃▄▅▆▇█ (used by the renderer).
pub const SPARK_LEVELS: u8 = 8;

#[derive(Debug, Clone, Copy)]
enum Sample {
    /// FUSE op completed with a latency and outcome.
    Completion {
        mono_us: u64,
        latency_us: u64,
        ok: bool,
    },
    /// A cache hit short-circuited the path; counted toward total ops
    /// and toward the cache-hit ratio but has no provider latency.
    CacheHit { mono_us: u64 },
}

impl Sample {
    fn mono_us(self) -> u64 {
        match self {
            Self::Completion { mono_us, .. } | Self::CacheHit { mono_us } => mono_us,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct MountWindow {
    samples: VecDeque<Sample>,
}

impl MountWindow {
    pub fn record_completion(&mut self, mono_us: u64, latency_us: u64, outcome: InspectorOutcome) {
        self.push(Sample::Completion {
            mono_us,
            latency_us,
            ok: outcome == InspectorOutcome::Ok,
        });
    }

    pub fn record_cache_hit(&mut self, mono_us: u64) {
        self.push(Sample::CacheHit { mono_us });
    }

    fn push(&mut self, sample: Sample) {
        self.samples.push_back(sample);
        self.evict(sample.mono_us());
    }

    fn evict(&mut self, now_mono: u64) {
        let cutoff = now_mono.saturating_sub(WINDOW_US);
        while self.samples.front().is_some_and(|s| s.mono_us() < cutoff) {
            self.samples.pop_front();
        }
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    /// Events per second averaged over the in-window samples. Returns 0
    /// when the window is empty or holds fewer than two timestamps far
    /// enough apart to give a meaningful denominator.
    #[allow(clippy::cast_precision_loss)]
    pub fn event_rate_per_sec(&self, now_mono: u64) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }
        let oldest = self.samples.front().map_or(now_mono, |s| s.mono_us());
        let span_micros = now_mono.saturating_sub(oldest);
        if span_micros < 1_000 {
            // less than 1ms of spread; not enough to project a rate
            return 0.0;
        }
        let span_seconds = span_micros as f64 / 1_000_000.0;
        self.samples.len() as f64 / span_seconds
    }

    /// Fraction of completions in the window that errored. Cache hits
    /// don't contribute (they're never errors in our wire model).
    #[allow(clippy::cast_precision_loss)]
    pub fn error_rate(&self) -> f64 {
        let (total, errors) = self
            .samples
            .iter()
            .filter_map(|s| match s {
                Sample::Completion { ok, .. } => Some(*ok),
                Sample::CacheHit { .. } => None,
            })
            .fold((0usize, 0usize), |(t, e), ok| {
                (t + 1, if ok { e } else { e + 1 })
            });
        if total == 0 {
            0.0
        } else {
            errors as f64 / total as f64
        }
    }

    /// Fraction of ops that hit the host cache. `None` when no ops at
    /// all (avoids printing a misleading 0%).
    #[allow(clippy::cast_precision_loss)]
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let hits = self
            .samples
            .iter()
            .filter(|s| matches!(s, Sample::CacheHit { .. }))
            .count();
        Some(hits as f64 / self.samples.len() as f64)
    }

    /// 95th-percentile completion latency in micros. `None` when no
    /// completions in the window.
    pub fn p95_latency_us(&self) -> Option<u64> {
        let mut latencies: Vec<u64> = self
            .samples
            .iter()
            .filter_map(|s| match s {
                Sample::Completion { latency_us, .. } => Some(*latency_us),
                Sample::CacheHit { .. } => None,
            })
            .collect();
        if latencies.is_empty() {
            return None;
        }
        latencies.sort_unstable();
        let idx = latencies.len().saturating_mul(95).div_ceil(100);
        let idx = idx.saturating_sub(1).min(latencies.len() - 1);
        Some(latencies[idx])
    }

    /// Sparkline buckets: `buckets` levels in [0, SPARK_LEVELS-1],
    /// each representing one time slice of the window. Newer is right.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    pub fn sparkline(&self, now_mono: u64, buckets: usize) -> Vec<u8> {
        if buckets == 0 {
            return Vec::new();
        }
        let bucket_us = (WINDOW_US / buckets as u64).max(1);
        let window_start = now_mono.saturating_sub(WINDOW_US);
        let mut counts = vec![0u32; buckets];
        for sample in &self.samples {
            let mono = sample.mono_us();
            if mono < window_start {
                continue;
            }
            let offset_us = mono.saturating_sub(window_start);
            let mut idx = (offset_us / bucket_us) as usize;
            if idx >= buckets {
                idx = buckets - 1;
            }
            counts[idx] = counts[idx].saturating_add(1);
        }
        let max = counts.iter().copied().max().unwrap_or(0);
        if max == 0 {
            return counts.into_iter().map(|_| 0).collect();
        }
        let max_levels = u32::from(SPARK_LEVELS - 1);
        counts
            .into_iter()
            .map(|c| {
                let scaled = c.saturating_mul(max_levels) / max;
                u8::try_from(scaled).unwrap_or(SPARK_LEVELS - 1)
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Render the 0..7 levels as Unicode block sparkline characters.
pub fn render_sparkline(buckets: &[u8]) -> String {
    const CHARS: [char; SPARK_LEVELS as usize] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    buckets
        .iter()
        .map(|&b| CHARS[(b as usize).min(CHARS.len() - 1)])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_window_aggregates() {
        let mut w = MountWindow::default();
        w.record_completion(0, 10_000, InspectorOutcome::Ok);
        w.record_completion(WINDOW_US + 1, 10_000, InspectorOutcome::Ok);
        assert_eq!(w.samples.len(), 1, "old sample should be evicted");

        let mut w = MountWindow::default();
        w.record_completion(1, 10_000, InspectorOutcome::Ok);
        w.record_cache_hit(2);
        w.record_cache_hit(3);
        w.record_completion(4, 10_000, InspectorOutcome::Ok);
        assert!((w.cache_hit_ratio().unwrap() - 0.5).abs() < 1e-6);

        let mut w = MountWindow::default();
        for us in [1_000, 2_000, 3_000, 4_000, 50_000] {
            w.record_completion(us, us, InspectorOutcome::Ok);
        }
        w.record_cache_hit(100_000);
        assert_eq!(w.p95_latency_us(), Some(50_000));
    }
}
