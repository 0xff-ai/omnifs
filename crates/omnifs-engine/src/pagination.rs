//! Host-synthesized directory pagination.
//!
//! A provider listing that carries a resume `next-cursor` is exposed with two
//! synthetic control files, `@next` and `@all`. Reading `@next` loads the next
//! page, appends its entries to the directory's accumulated dirents, advances
//! the stored cursor, and invalidates the directory so a later `readdir`
//! reflects the grown listing. Reading `@all` repeats `@next` to exhaustion or
//! the [`MAX_PAGINATION_PAGES`] safety cap.
//!
//! # Recursion behavior under tree walks
//!
//! The control files are never directories (they are regular files), so
//! `find`/`fd`/`grep -r` never descend *through* them. They are listed regular
//! files, though, so a recursive content search can `open`/`read` them:
//!
//! - Under ignore-respecting tools (`rg`, `fd`, git), the mount-root
//!   `.gitignore`/`.ignore`/`.rgignore` files (each `@*`) hide the controls, so
//!   a tree walk never triggers pagination. This is fully safe.
//! - Under tools that ignore those files (e.g. GNU `grep -r`), a walk can read
//!   the controls. The materialization is *bounded*, not unbounded: a single
//!   `@next` advances exactly one page, and a single `@all` is capped at
//!   [`MAX_PAGINATION_PAGES`]. A walk therefore cannot expand a feed without
//!   limit; the worst case is one extra page (or the cap) per control read.

use super::Runtime;
use crate::cache::{Record as CacheRecord, RecordKind};
use crate::ops::namespace::{DirEntry, ListOutcome};
use crate::view::{
    DirentRecord, DirentsPayload, EntryMeta, FileAttrsCache, FileSize, ReadMode, Stability,
};
use omnifs_api::events::TraceId;
use omnifs_core::path::Path;
use tracing::warn;

/// Synthetic control-file leaf that loads the next page of a paged directory.
pub const CTRL_NEXT: &str = "@next";
/// Synthetic control-file leaf that loads every remaining page (capped).
pub const CTRL_ALL: &str = "@all";
/// Reserved prefix for host control entries. A provider listing must never
/// yield a child whose name starts with this; such entries are skipped so
/// provider data cannot shadow `@next`/`@all`.
pub const CTRL_PREFIX: char = '@';

/// Safety cap on `@all`: never load more than this many pages in one shot, so a
/// runaway or adversarial feed can't materialize unboundedly.
pub const MAX_PAGINATION_PAGES: u32 = 20;

/// True when `name` is one of the synthetic control-file leaves.
#[must_use]
pub fn is_control_name(name: &str) -> bool {
    name == CTRL_NEXT || name == CTRL_ALL
}

/// True when a provider listing must not use this leaf name (`@` is reserved
/// for host-synthesized control entries and mount-root ignore patterns).
#[must_use]
pub fn is_reserved_provider_leaf(name: &str) -> bool {
    name.starts_with(CTRL_PREFIX)
}

/// Mount-root ignore files. Each carries a single `@*` pattern so recursive
/// tools that honor ignore files (`rg`, `fd`, git) skip the `@next`/`@all`
/// control files by default and never trigger pagination during a tree walk.
pub const IGNORE_FILES: [&str; 3] = [".gitignore", ".ignore", ".rgignore"];

/// Content served for any mount-root ignore file: ignore every `@`-prefixed
/// control entry.
pub const IGNORE_CONTENT: &str = "@*\n";

/// True when `name` is one of the mount-root ignore files.
#[must_use]
pub fn is_ignore_name(name: &str) -> bool {
    IGNORE_FILES.contains(&name)
}

/// Outcome of advancing a paged directory by one page.
pub enum NextPageOutcome {
    /// A page was loaded. `added` is the count of newly appended entries;
    /// `more` is true when the feed still has further pages.
    Loaded { added: usize, more: bool },
    /// The directory had no resume cursor (not paged, or already exhausted).
    NoMore,
    /// The provider failed; the stored dirents were left untouched. The string
    /// is a human-readable error line.
    Error(String),
}

impl Runtime {
    /// Append `@next`/`@all` control dirents to a freshly built dirents payload
    /// when the listing carried a resume cursor, and drop any provider entry
    /// whose name collides with the reserved `@` prefix.
    ///
    /// Used by both the FUSE snapshot builder and the continuation path so the
    /// control entries appear identically everywhere the directory is listed.
    pub fn control_entries() -> [DirentRecord; 2] {
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

    /// Load the next page for paged directory `path`, accumulating its entries
    /// into the cached dirents record and advancing the stored cursor.
    ///
    /// Reads the directory's accumulated `DirentsPayload` from the view cache;
    /// if it has no `next_cursor`, returns [`NextPageOutcome::NoMore`].
    /// Otherwise echoes the cursor to `list_children` (with no validator: this
    /// is pagination, not revalidation), strips any existing `@next`/`@all`,
    /// appends the new entries (and fresh control entries iff the new page
    /// still has a cursor), re-stores the payload, and invalidates `path` so
    /// the kernel re-lists.
    ///
    /// On a provider error the stored dirents are left untouched.
    pub async fn paginate_next(&self, path: &Path, trace: Option<TraceId>) -> NextPageOutcome {
        // Serialize the read-modify-write of this directory's accumulated
        // dirents. Without this, two concurrent `@next` reads could both
        // snapshot the same base record below and each append only their own
        // page, dropping the other's (e.g. page0 lost when page1 and page2
        // race). The lock is held across the continuation fetch and store so
        // the whole RMW is atomic per directory. Driven only by `block_on` on
        // the FUSE thread, so contention is brief and deadlock-free.
        let lock = self.pagination_lock(path);
        let _guard = lock.lock().await;

        let Some(record) = self.cache.cache_get(path, RecordKind::Dirents, None) else {
            return NextPageOutcome::NoMore;
        };
        let Some(mut dirents) = DirentsPayload::deserialize(&record.payload) else {
            return NextPageOutcome::NoMore;
        };
        let Some(cursor) = dirents.next_cursor.clone() else {
            return NextPageOutcome::NoMore;
        };
        // This record is, by construction, a host-accumulated paginated
        // listing. Keep the marker set so a completed accumulation is served
        // from cache (and not refetched as page 0) after the cursor clears.
        dirents.paginated = true;

        // Echo the cursor; no validator on a continuation page.
        let listing = match self
            .namespace()
            .list_children(path, None, Some(cursor), trace)
            .await
        {
            Ok(ListOutcome::Entries(listing)) => listing,
            Ok(ListOutcome::Unchanged) => {
                // A continuation that resolves to `unchanged` means the feed is
                // stable; treat as exhausted. Mirror the terminal branch: clear
                // the cursor, strip the now-dead `@next`/`@all` controls, mark
                // the accumulation complete, re-store, and notify the kernel so
                // a subsequent `ls` reflects the control-free listing.
                dirents.next_cursor = None;
                strip_control_entries(&mut dirents.entries);
                self.store_paginated_dirents(path, &dirents);
                // The feed is complete (no cursor); drop the per-path lock so the
                // map stays bounded. Safe: a later `paginate_next` finds no cursor
                // and returns `NoMore` without an RMW, so it needs no lock.
                self.prune_pagination_lock(path);
                self.record_dir_changed(path);
                return NextPageOutcome::Loaded {
                    added: 0,
                    more: false,
                };
            },
            Ok(ListOutcome::Subtree(_)) => {
                return NextPageOutcome::Error(
                    "pagination target resolved to a subtree handoff\n".to_string(),
                );
            },
            Err(error) => {
                return NextPageOutcome::Error(format!("{error}\n"));
            },
        };

        // The continuation `list_children` above does NOT write the directory's
        // dirents record (it suppresses the authoritative write for continuation
        // pages), so the accumulated payload we snapshotted before the call is
        // still the cached listing. Append this page's entries to it and store.
        strip_control_entries(&mut dirents.entries);
        let added = append_listing_entries(&mut dirents.entries, &listing.entries);

        let more = listing.next_cursor.is_some();
        dirents.next_cursor.clone_from(&listing.next_cursor);
        // The accumulated listing is exhaustive only once the feed completes.
        dirents.exhaustive = !more && listing.exhaustive;
        if more {
            dirents.entries.extend(Self::control_entries());
        }

        self.store_paginated_dirents(path, &dirents);
        if !more {
            // Feed exhausted (cursor cleared); drop the per-path lock so the map
            // stays bounded. Safe: a later `paginate_next` finds no cursor and
            // returns `NoMore` without an RMW, so it needs no lock.
            self.prune_pagination_lock(path);
        }
        // Notify the kernel to re-read the directory; the grown listing is
        // served from the accumulated view dirents we just stored. We must NOT
        // delete the view record here (that would wipe the accumulation).
        self.record_dir_changed(path);

        NextPageOutcome::Loaded { added, more }
    }

    /// Loop [`paginate_next`](Self::paginate_next) until the feed is exhausted
    /// or [`MAX_PAGINATION_PAGES`] pages have been loaded. Returns a one-line
    /// summary suitable for use as the `@all` file content.
    pub async fn paginate_all(&self, path: &Path, trace: Option<TraceId>) -> String {
        let mut pages: u32 = 0;
        let mut added_total: usize = 0;
        loop {
            if pages >= MAX_PAGINATION_PAGES {
                return format!(
                    "loaded {pages} pages (+{added_total}); capped at {MAX_PAGINATION_PAGES} pages\n"
                );
            }
            match self.paginate_next(path, trace).await {
                NextPageOutcome::Loaded { added, more } => {
                    pages += 1;
                    added_total += added;
                    if !more {
                        return format!("loaded {pages} pages (+{added_total}); complete\n");
                    }
                },
                NextPageOutcome::NoMore => {
                    if pages == 0 {
                        return "no more pages\n".to_string();
                    }
                    return format!("loaded {pages} pages (+{added_total}); complete\n");
                },
                NextPageOutcome::Error(message) => {
                    if pages == 0 {
                        return message;
                    }
                    return format!("loaded {pages} pages (+{added_total}); error: {message}");
                },
            }
        }
    }

    /// Per-path lock guarding pagination's accumulated-dirents read-modify-write.
    /// One `Arc<Mutex<()>>` per directory path, created on first use.
    fn pagination_lock(&self, path: &Path) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.pagination_locks
            .entry(path.to_string())
            .or_default()
            .clone()
    }

    /// Drop the per-path pagination lock to keep [`pagination_locks`] bounded.
    ///
    /// Called only when the feed has just exhausted (cursor cleared) while the
    /// caller still holds the guard. Removing the map's `Arc` is safe: any
    /// already-queued waiter holds its own `Arc` clone and still serializes
    /// against the current holder; a *new* caller that arrives after this
    /// allocates a fresh lock, but finds no cursor and returns `NoMore` without
    /// any read-modify-write, so it has nothing to serialize.
    ///
    /// [`pagination_locks`]: Runtime::pagination_locks
    fn prune_pagination_lock(&self, path: &Path) {
        self.pagination_locks.remove(path.as_str());
    }

    fn store_paginated_dirents(&self, path: &Path, dirents: &DirentsPayload) {
        if let Some(payload) = dirents.serialize() {
            let record = CacheRecord::new(RecordKind::Dirents, payload);
            self.cache
                .cache_put(path, RecordKind::Dirents, None, &record);
        }
    }
}

/// A small dynamic file: each `cat` re-fires the control action and directory
/// recursion never descends through it.
fn control_entry_meta() -> EntryMeta {
    EntryMeta::file(
        FileAttrsCache::deferred(FileSize::Unknown, ReadMode::Full, Stability::Dynamic, None)
            .expect("control attrs are valid"),
    )
}

/// Attrs for a control file once its status content has been generated at open.
/// The lookup-time [`control_entry_meta`] reports `Unknown` (the message length
/// is not known until the action runs); promoting to an exact size at open lets
/// `cat` (which sizes its reads against `st_size`) read the whole status instead
/// of the `Unknown` placeholder's single byte. Mirrors the learned-size
/// promotion the regular full-read path applies.
pub fn control_read_attrs(len: u64) -> FileAttrsCache {
    FileAttrsCache::deferred(
        FileSize::Exact(len),
        ReadMode::Full,
        Stability::Dynamic,
        None,
    )
    .expect("control read attrs are valid")
}

/// Drop any `@`-prefixed control entries from an accumulated dirents list.
fn strip_control_entries(entries: &mut Vec<DirentRecord>) {
    entries.retain(|e| !is_reserved_provider_leaf(&e.name));
}

/// Append `new_entries` to `entries`, skipping names already present and
/// reserved `@`-prefixed names (which a provider must not emit). Returns the
/// number of entries actually appended.
fn append_listing_entries(entries: &mut Vec<DirentRecord>, new_entries: &[DirEntry]) -> usize {
    let mut added = 0;
    for entry in new_entries {
        if is_reserved_provider_leaf(&entry.name) {
            warn!(
                name = entry.name.as_str(),
                "provider listing yielded a reserved '@'-prefixed entry; skipping"
            );
            continue;
        }
        if entries.iter().any(|e| e.name == entry.name) {
            continue;
        }
        entries.push(DirentRecord {
            name: entry.name.clone(),
            meta: entry.meta.clone(),
        });
        added += 1;
    }
    added
}
