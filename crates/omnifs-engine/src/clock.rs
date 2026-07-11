//! Host clock and TTL constants for cache freshness and negative records.

/// Default deadline for a dynamic view leaf and a negative record, in milliseconds.
pub const DYNAMIC_TTL_MILLIS: u64 = 3_000;

/// Wall-clock milliseconds since the Unix epoch for cache deadlines and negatives.
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
