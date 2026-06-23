//! NFS-renderer cache helper that remains after the read/list/lookup decision
//! logic moved into `omnifs-tree`.
//!
//! The adapter still probes cached dirents for a positive child because NFS
//! flattens a directory into a finite snapshot and may receive lookups for names
//! that were already seen in that snapshot.

use omnifs_cache::Record as CacheRecord;
use omnifs_core::view::{self as cache, EntryMeta, FileAttrsCache};

#[derive(Debug, Clone)]
pub(crate) enum LookupCacheHit {
    Positive(EntryMeta),
    Negative,
}

pub(crate) fn cached_dirent_lookup(record: &CacheRecord, name: &str) -> Option<LookupCacheHit> {
    let dirents = cache::DirentsPayload::deserialize(&record.payload)?;
    if let Some(entry) = dirents.entries.iter().find(|entry| entry.name == name) {
        return Some(LookupCacheHit::Positive(entry.meta.clone()));
    }
    dirents.exhaustive.then_some(LookupCacheHit::Negative)
}

/// Keep a learned exact size on the NFS inode across an origin-agnostic refresh:
/// a re-listing that projects a kind-derived placeholder must not erase a size
/// learned from a complete read. Returns the attrs the inode should hold after
/// merging `incoming` over `existing`. NFS keeps this renderer-side, exactly as
/// the FUSE inode does.
pub(crate) fn merge_file_attrs(
    existing: Option<&FileAttrsCache>,
    incoming: Option<FileAttrsCache>,
) -> Option<FileAttrsCache> {
    FileAttrsCache::merge_preserving_learned_size(existing, incoming)
}
