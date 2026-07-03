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
//! This module is the source of truth for the synthetic projection surface:
//! control names, ignore-file names and bytes, metadata, and name predicates.

use crate::Runtime;
use crate::view::{DirentRecord, EntryMeta, FileAttrsCache, FileSize, ReadMode, Stability};
use omnifs_core::path::Path;

use super::node::{Entry, PaginationControl, Synthetic, SyntheticContent};

/// Synthetic control-file leaf that loads the next page of a paged directory.
const CTRL_NEXT: &str = "@next";
/// Synthetic control-file leaf that loads every remaining page (capped).
const CTRL_ALL: &str = "@all";
/// Reserved prefix for host control entries. A provider listing must never
/// yield a child whose name starts with this; such entries are skipped so
/// provider data cannot shadow `@next`/`@all`.
const CTRL_PREFIX: char = '@';

/// Mount-root ignore files. Each carries a single `@*` pattern so recursive
/// tools that honor ignore files (`rg`, `fd`, git) skip the `@next`/`@all`
/// control files by default and never trigger pagination during a tree walk.
pub const IGNORE_FILES: [&str; 3] = [".gitignore", ".ignore", ".rgignore"];

/// Content served for any mount-root ignore file: ignore every `@`-prefixed
/// control entry.
pub const IGNORE_CONTENT: &str = "@*\n";

/// True when `name` is one of the synthetic control-file leaves.
#[must_use]
pub(crate) fn is_control_name(name: &str) -> bool {
    name == CTRL_NEXT || name == CTRL_ALL
}

/// True when a provider listing must not use this leaf name (`@` is reserved
/// for host-synthesized control entries and mount-root ignore patterns).
#[must_use]
pub fn is_reserved_provider_leaf(name: &str) -> bool {
    name.starts_with(CTRL_PREFIX)
}

/// True when `name` is one of the mount-root ignore files.
#[must_use]
fn is_ignore_name(name: &str) -> bool {
    IGNORE_FILES.contains(&name)
}

/// The cached dirent records for pagination controls a paged directory carries.
pub(crate) fn control_entries() -> [DirentRecord; 2] {
    [
        DirentRecord {
            name: CTRL_NEXT.to_string(),
            meta: control_entry_meta(),
        },
        DirentRecord {
            name: CTRL_ALL.to_string(),
            meta: control_entry_meta(),
        },
    ]
}

/// Attrs for a control file once its status content has been generated at open.
/// The lookup-time [`control_entry_meta`] reports `Unknown` (the message length
/// is not known until the action runs); promoting to an exact size at open lets
/// `cat` (which sizes its reads against `st_size`) read the whole status instead
/// of the `Unknown` placeholder's single byte. Mirrors the learned-size
/// promotion the regular full-read path applies.
pub(crate) fn control_read_attrs(len: u64) -> FileAttrsCache {
    FileAttrsCache::deferred(
        FileSize::Exact(len),
        ReadMode::Full,
        Stability::Dynamic,
        None,
    )
    .expect("control read attrs are valid")
}

/// A small dynamic file: each `cat` re-fires the control action and directory
/// recursion never descends through it.
fn control_entry_meta() -> EntryMeta {
    EntryMeta::file(
        FileAttrsCache::deferred(FileSize::Unknown, ReadMode::Full, Stability::Dynamic, None)
            .expect("control attrs are valid"),
    )
}

impl PaginationControl {
    /// Map a control name to its pagination action. `None` for any other name.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            CTRL_NEXT => Some(Self::Next),
            CTRL_ALL => Some(Self::All),
            _ => None,
        }
    }

    /// The two pagination-control `Entry`s a paged directory's listing carries.
    /// Reuses the cached dirent records (the same `@next`/`@all` names and
    /// `control_entry_meta`) so the renderer-facing surface is identical to
    /// what pagination accumulates into cached dirents.
    pub(crate) fn entries() -> Vec<Entry> {
        control_entries()
            .into_iter()
            .filter_map(|record| {
                Self::from_name(&record.name).map(|action| {
                    Entry::synthetic(
                        record.name,
                        record.meta,
                        Synthetic::pagination_control(action),
                    )
                })
            })
            .collect()
    }
}

impl Synthetic {
    /// File `EntryMeta` for a synthetic mount-root ignore file. Its size is
    /// exact (the ignore content is fixed) so `ls -l`/`cat` report the right
    /// length without a learned-size round trip.
    fn root_ignore_meta() -> EntryMeta {
        EntryMeta::file(
            FileAttrsCache::deferred(
                FileSize::Exact(IGNORE_CONTENT.len() as u64),
                ReadMode::Full,
                Stability::Stable,
                None,
            )
            .expect("root ignore attrs are valid"),
        )
    }

    /// The synthetic descriptor for an ignore file: its fixed bytes.
    fn root_ignore() -> Self {
        Self {
            content: SyntheticContent::Fixed(IGNORE_CONTENT.as_bytes().to_vec()),
        }
    }

    fn pagination_control(action: PaginationControl) -> Self {
        Self {
            content: SyntheticContent::PaginationControl(action),
        }
    }

    fn root_ignore_entry(name: &str) -> Entry {
        Entry::synthetic(
            name.to_string(),
            Self::root_ignore_meta(),
            Self::root_ignore(),
        )
    }

    /// The mount-root ignore `Entry`s to append to a root listing, skipping any
    /// name the provider already projects (a real `.gitignore` wins and is NOT
    /// shadowed).
    pub(crate) fn root_ignore_entries(existing: &[Entry]) -> Vec<Entry> {
        IGNORE_FILES
            .iter()
            .filter(|name| !existing.iter().any(|e| &e.name == *name))
            .map(|name| Self::root_ignore_entry(name))
            .collect()
    }
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
        let action = PaginationControl::from_name(name)?;
        // A control resolves only while the parent's accumulated dirents still
        // carry it (a resume cursor remains). Probe the view cache for the
        // control dirent; absent => the feed is exhausted and the control is
        // gone, so the caller surfaces NotFound.
        let dirent = cached_control_dirent(runtime, parent, name)?;
        return Some((dirent.meta, Synthetic::pagination_control(action)));
    }

    if is_ignore_name(name) && parent.is_root() && !provider_has_real {
        return Some((Synthetic::root_ignore_meta(), Synthetic::root_ignore()));
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
) -> Option<crate::view::DirentRecord> {
    use crate::cache::RecordKind;
    use crate::view::DirentsPayload;

    let dirents = if let Some(record) = runtime.cache().mem_get(parent, RecordKind::Dirents, None) {
        DirentsPayload::deserialize(&record.payload)?
    } else {
        let record = runtime
            .cache()
            .cache_get(parent, RecordKind::Dirents, None)?;
        DirentsPayload::deserialize(&record.payload)?
    };
    dirents.entries.into_iter().find(|e| e.name == name)
}
