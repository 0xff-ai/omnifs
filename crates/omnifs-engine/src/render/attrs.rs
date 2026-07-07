/// Maximum whole-file payload the tree will materialize in memory for one read.
///
/// Ranged reads stream through [`crate::RangedHandle`] and are not subject to
/// this cap.
pub const MATERIALIZE_MAX_BYTES: u64 = 64 * 1024 * 1024;
