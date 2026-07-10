//! Host-side pagination actions and accumulated dirents.
//!
//! A provider listing that carries a resume `next-cursor` is exposed with two
//! synthetic control files, `@next` and `@all`. Reading `@next` loads the next
//! page, appends its entries to the directory's accumulated dirents, advances
//! the stored cursor, and invalidates the directory so a later `readdir`
//! reflects the grown listing. Reading `@all` repeats `@next` to exhaustion or
//! the [`MAX_PAGINATION_PAGES`] safety cap.
//!
//! The `@next`/`@all` dirent records are never stripped back out of the
//! accumulated payload once a directory pages, even after the cursor clears.
//! That is what lets a name a consumer already captured from an earlier
//! listing snapshot keep resolving and reading (as a no-op) after the feed
//! exhausts: presence in an already-served listing must never regress to
//! ENOENT (the converse of the listing-authority rule that absence from a
//! non-exhaustive listing is never ENOENT either). A FRESH listing still stops
//! naming the controls once `next_cursor` clears (`tree::list::listing_from_dirents`),
//! so the two facts stay separate: `next_cursor` gates what a new `readdir`
//! shows, the persisted control records gate what a name resolves to.
//!
//! `tree::synthetic` owns the names, attrs, ignore files, and reserved-name
//! predicates for those projection entries. This module owns only the runtime
//! action and cache-backed accumulation once a control file is read.
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
use crate::tree::synthetic::{control_entries, is_reserved_provider_leaf};
use crate::view::{DirentRecord, DirentsPayload};
use omnifs_api::events::TraceId;
use omnifs_core::path::Path;
use tracing::warn;

/// Safety cap on `@all`: never load more than this many pages in one shot, so a
/// runaway or adversarial feed can't materialize unboundedly.
pub const MAX_PAGINATION_PAGES: u32 = 20;

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
    /// Load the next page for paged directory `path`, accumulating its entries
    /// into the cached dirents record and advancing the stored cursor.
    ///
    /// Reads the directory's accumulated `DirentsPayload` from the view cache;
    /// if it has no `next_cursor`, returns [`NextPageOutcome::NoMore`].
    /// Otherwise echoes the cursor to `list_children` (with no validator: this
    /// is pagination, not revalidation), strips the existing `@next`/`@all`
    /// records, appends the new entries plus a fresh pair of control records
    /// (the records persist regardless of whether the new page still has a
    /// cursor, so a name already resolved from an earlier snapshot keeps
    /// resolving after exhaustion), re-stores the payload, and invalidates
    /// `path` so the kernel re-lists.
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
        let Some(cursor) = dirents.next_cursor.take() else {
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
                // the cursor (the `@next`/`@all` records already stored stay put,
                // so they keep resolving), re-store, and notify the kernel so a
                // subsequent `ls` reflects the now-control-free fresh listing.
                dirents.next_cursor = None;
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
        // Re-add the control records unconditionally, even on the terminal
        // page: they must persist so a name a consumer already resolved from
        // an earlier (non-exhausted) listing snapshot keeps resolving. A FRESH
        // listing separately stops naming them once `next_cursor` clears.
        dirents.entries.extend(control_entries());

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
