//! Renderer-neutral synthetic-entry construction.
//!
//! `Tree` owns the host-synthesized entries that no provider projects so every
//! renderer presents them identically:
//!
//! - the mount-root ignore files (`.gitignore`/`.ignore`/`.rgignore`), static
//!   bytes that hide the `@`-prefixed control files and generated provider
//!   README leaves from ignore-respecting tree walks;
//! - the pagination controls (`@next`/`@all`), an action whose read advances the
//!   parent directory's accumulated dirents.
//!
//! This module is the source of truth for the synthetic projection surface:
//! control names, ignore-file names and bytes, metadata, and name predicates.

use crate::cache::MountResources;
use crate::view::{DirentRecord, EntryMeta, FileAttrsCache, FileSize, ReadMode, Stability};
use omnifs_core::path::Path;

use super::error::{Result, TreeError};
use super::node::{Entry, PaginationControl, Synthetic, SyntheticContent};

/// Synthetic control-file leaf that loads the next page of a paged directory.
const CTRL_NEXT: &str = "@next";
/// Synthetic control-file leaf that loads every remaining page (capped).
const CTRL_ALL: &str = "@all";
/// Mount-root ignore files. Each carries patterns so recursive tools that
/// honor ignore files (`rg`, `fd`, git) skip the `@next`/`@all` control files
/// and generated README leaves by default.
pub const IGNORE_FILES: [&str; 3] = [".gitignore", ".ignore", ".rgignore"];

/// Content served for any mount-root ignore file: ignore the two pagination
/// controls, the provider-root README, and one top-level branch README.
pub const IGNORE_CONTENT: &str = "@next\n@all\n/README.md\n/*/README.md\n";

/// True when `name` is one of the synthetic control-file leaves.
#[must_use]
pub(crate) fn is_control_name(name: &str) -> bool {
    name == CTRL_NEXT || name == CTRL_ALL
}

/// True when a provider listing would shadow a host-synthesized control leaf.
#[must_use]
pub fn is_reserved_provider_leaf(name: &str) -> bool {
    is_control_name(name)
}

/// True when `name` is a host-owned ignore file at a mount root.
///
/// This predicate is intentionally separate from [`is_reserved_provider_leaf`]:
/// providers may project these names below the root, while the host owns them
/// only at the mount root.
#[must_use]
pub(crate) fn is_root_ignore_name(name: &str) -> bool {
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

    /// The mount-root ignore `Entry`s to append to a root listing.
    pub(crate) fn root_ignore_entries() -> Vec<Entry> {
        IGNORE_FILES
            .iter()
            .map(|name| Self::root_ignore_entry(name))
            .collect()
    }
}

/// Resolve a synthetic child by name against its parent. Returns the
/// `(meta, synthetic)` pair when `name` is a host-synthesized entry that should
/// resolve at `parent`:
///
/// - a mount-root ignore file at the mount root (the host owns these names even
///   when a provider projects a colliding entry);
/// - a pagination control whenever `parent`'s cached dirents have EVER carried
///   it, looked up from the host view cache.
///
/// The control branch reads the parent's cached dirents directly. `pagination.rs`
/// never strips a `@next`/`@all` record back out once accumulated, even after
/// the cursor clears, so the control keeps resolving for the directory's whole
/// cached lifetime: a name a consumer captured from an earlier (non-exhausted)
/// listing snapshot never regresses to ENOENT. Only a directory that never
/// paged (no cached dirents, or cached dirents that never carried a control)
/// surfaces `None` here.
pub(crate) fn resolve_synthetic_child(
    resources: &MountResources,
    parent: &Path,
    name: &str,
) -> Result<Option<(EntryMeta, Synthetic)>> {
    if parent.is_root() && is_root_ignore_name(name) {
        return Ok(Some((
            Synthetic::root_ignore_meta(),
            Synthetic::root_ignore(),
        )));
    }

    if is_control_name(name) {
        let Some(action) = PaginationControl::from_name(name) else {
            return Ok(None);
        };
        // Probe the view cache for the control dirent; absent => this
        // directory never paged (or its cached dirents were evicted), so the
        // caller surfaces NotFound. Present => resolves regardless of whether
        // a resume cursor remains (the record outlives exhaustion).
        let Some(dirent) = cached_control_dirent(resources, parent, name)? else {
            return Ok(None);
        };
        return Ok(Some((dirent.meta, Synthetic::pagination_control(action))));
    }

    Ok(None)
}

/// Find a `@next`/`@all` dirent in the parent directory's cached dirents record.
/// `None` when the parent has never paged (its cached dirents, if any, never
/// carried a control record); a directory that has ever paged keeps the record
/// regardless of cursor state, so this does NOT go `None` merely because the
/// feed exhausted.
fn cached_control_dirent(
    resources: &MountResources,
    parent: &Path,
    name: &str,
) -> Result<Option<crate::view::DirentRecord>> {
    use crate::cache::RecordKind;
    use crate::view::DirentsPayload;

    let record = resources
        .cache_get(parent, RecordKind::Dirents, None)
        .map_err(|error| TreeError::internal(error.to_string()))?;
    let Some(record) = record else {
        return Ok(None);
    };
    let dirents: DirentsPayload = postcard::from_bytes(&record.payload)
        .map_err(|error| TreeError::internal(error.to_string()))?;
    Ok(dirents.entries.into_iter().find(|e| e.name == name))
}
