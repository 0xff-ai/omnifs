//! Renderer-neutral synthetic-entry construction.
//!
//! `Tree` owns the host-synthesized entries that no provider projects so every
//! renderer presents them identically:
//!
//! - the mount-root ignore files (`.gitignore`/`.ignore`/`.rgignore`), static
//!   `@*\n` bytes that hide the `@`-prefixed control files from ignore-respecting
//!   tree walks;
//! - the pagination controls (`@next`/`@all`), an action whose read advances the
//!   parent directory's accumulated dirents.
//!
//! All the names, metas, and byte content reuse the host `pagination`
//! source-of-truth constants (`IGNORE_FILES`, `IGNORE_CONTENT`,
//! `control_entries`, `is_*` predicates), so the synthetic surface is defined in
//! exactly one place and the FUSE/NFS copies collapse onto it.

use omnifs_core::path::Path;
use omnifs_core::view::{ByteSource, EntryMeta, FileAttrsCache, FileSize, ReadMode, Stability};
use omnifs_host::Runtime;
use omnifs_host::pagination::{
    self, CTRL_ALL, CTRL_NEXT, IGNORE_CONTENT, IGNORE_FILES, is_control_name, is_ignore_name,
};

use crate::node::{Entry, PaginationControl, Synthetic, SyntheticContent};

/// True when `path` is the mount root (the only directory where ignore files are
/// synthesized). Mount-relative paths are rooted at `/`.
pub(crate) fn is_mount_root(path: &Path) -> bool {
    path.is_root()
}

/// Live-file `EntryMeta` for a synthetic mount-root ignore file. Its size is
/// exact (the ignore content is fixed) so `ls -l`/`cat` report the right length
/// without a learned-size round trip. Mirrors the FUSE `root_ignore_meta`.
pub(crate) fn root_ignore_meta() -> EntryMeta {
    EntryMeta::file(FileAttrsCache {
        size: FileSize::Exact(IGNORE_CONTENT.len() as u64),
        bytes: ByteSource::Deferred(ReadMode::Full),
        stability: Stability::Live,
        version_token: None,
    })
}

/// The synthetic descriptor for an ignore file: its fixed bytes.
pub(crate) fn root_ignore_synthetic() -> Synthetic {
    Synthetic {
        content: SyntheticContent::Fixed(IGNORE_CONTENT.as_bytes().to_vec()),
    }
}

/// Map a control name to its pagination action. `None` for any other name.
pub(crate) fn control_action(name: &str) -> Option<PaginationControl> {
    match name {
        CTRL_NEXT => Some(PaginationControl::Next),
        CTRL_ALL => Some(PaginationControl::All),
        _ => None,
    }
}

/// The two pagination-control `Entry`s a paged directory's listing carries.
/// Reuses the host `Runtime::control_entries` dirent records (the same `@next`/
/// `@all` names and `control_entry_meta`) so the control surface is identical to
/// what the host accumulates into the cached dirents.
pub(crate) fn control_entries() -> Vec<Entry> {
    Runtime::control_entries()
        .into_iter()
        .filter_map(|record| {
            control_action(&record.name).map(|action| Entry {
                name: record.name,
                meta: record.meta,
                synthetic: Some(Synthetic {
                    content: SyntheticContent::PaginationControl(action),
                }),
            })
        })
        .collect()
}

/// The mount-root ignore `Entry`s to append to a root listing, skipping any name
/// the provider already projects (a real `.gitignore` wins and is NOT shadowed).
pub(crate) fn root_ignore_entries(existing: &[Entry]) -> Vec<Entry> {
    IGNORE_FILES
        .iter()
        .filter(|name| !existing.iter().any(|e| &e.name == *name))
        .map(|name| Entry {
            name: (*name).to_string(),
            meta: root_ignore_meta(),
            synthetic: Some(root_ignore_synthetic()),
        })
        .collect()
}

/// Resolve a synthetic child by name against its parent. Returns the
/// `(meta, synthetic)` pair when `name` is a host-synthesized entry that should
/// resolve at `parent`:
///
/// - a mount-root ignore file ONLY at the mount root, and ONLY when the provider
///   does not project a real one (`provider_has_real` is the caller's signal that
///   a provider lookup already resolved the name positively);
/// - a pagination control ONLY when `parent`'s cached dirents still carry it (a
///   resume cursor remains), looked up from the host view cache.
///
/// The control branch reads the parent's cached dirents directly so a control is
/// never resurrected after the feed exhausts (the FUSE `cached_control_dirent`
/// semantics: a control name absent from the cached dirents is ENOENT, never a
/// stale dedup-table hit).
pub(crate) fn resolve_synthetic_child(
    runtime: &Runtime,
    parent: &Path,
    name: &str,
    provider_has_real: bool,
) -> Option<(EntryMeta, Synthetic)> {
    if is_control_name(name) {
        let action = control_action(name)?;
        // A control resolves only while the parent's accumulated dirents still
        // carry it (a resume cursor remains). Probe the view cache for the
        // control dirent; absent => the feed is exhausted and the control is
        // gone, so the caller surfaces NotFound.
        let dirent = cached_control_dirent(runtime, parent, name)?;
        return Some((
            dirent.meta,
            Synthetic {
                content: SyntheticContent::PaginationControl(action),
            },
        ));
    }

    if is_ignore_name(name) && is_mount_root(parent) && !provider_has_real {
        return Some((root_ignore_meta(), root_ignore_synthetic()));
    }

    None
}

/// Find a `@next`/`@all` dirent in the parent directory's cached dirents record
/// (mem then unified cache). `None` when the parent is not paged or the control
/// is absent (feed exhausted). Mirrors the FUSE `cached_control_dirent`.
fn cached_control_dirent(
    runtime: &Runtime,
    parent: &Path,
    name: &str,
) -> Option<omnifs_core::view::DirentRecord> {
    use omnifs_cache::RecordKind;
    use omnifs_core::view::DirentsPayload;

    let dirents = if let Some(record) = runtime.mem_get(parent.as_str(), RecordKind::Dirents, None)
    {
        DirentsPayload::deserialize(&record.payload)?
    } else {
        let record = runtime.cache_get(parent.as_str(), RecordKind::Dirents, None)?;
        DirentsPayload::deserialize(&record.payload)?
    };
    dirents.entries.into_iter().find(|e| e.name == name)
}

// Re-export the reserved-prefix predicate so `list` can drop a provider entry
// that collides with the `@` namespace without importing `pagination` directly.
pub(crate) use pagination::is_reserved_provider_leaf;
