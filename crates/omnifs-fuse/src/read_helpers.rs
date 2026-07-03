//! Kernel-side read slicing.
//!
//! The read-path DECISION logic (payload resolution, the cache cascade, the
//! write fence, learned-size promotion, ranged-EOF learning) lives in
//! `omnifs-engine tree`'s `read`/`handle` modules; the FUSE adapter keeps only the
//! kernel offset/size slicing of an already-rendered whole-file buffer.

/// Slice `data` at the given FUSE `offset` and `size`, returning the relevant
/// byte range. Returns an empty slice when `offset` is past the end.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn data_slice(data: &[u8], offset: u64, size: u32) -> &[u8] {
    let start = offset as usize;
    let end = (start + size as usize).min(data.len());
    data.get(start..end).unwrap_or(&[])
}
