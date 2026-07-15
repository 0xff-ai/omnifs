//! Host clock and TTL constants for cache freshness and negative records.

use crate::view::Stability;

/// Default deadline for a dynamic view leaf and a negative record, in milliseconds.
pub const DYNAMIC_TTL_MILLIS: u64 = 3_000;

/// Translate provider stability into the single host-owned freshness policy.
pub(crate) fn freshness_expiry(stability: Stability, now_millis: u64) -> Option<u64> {
    match stability {
        Stability::Stable => None,
        Stability::Dynamic => Some(now_millis.saturating_add(DYNAMIC_TTL_MILLIS)),
        Stability::Live => Some(now_millis),
    }
}

/// Wall-clock milliseconds since the Unix epoch for cache deadlines and negatives.
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
