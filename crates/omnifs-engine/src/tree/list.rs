//! Internal directory listing types and provider-listing execution.
//!
//! This is the renderer-neutral listing policy shared by FUSE and NFS:
//!
//! - the authoritative-listing cache consult (mem then unified view cache),
//! - the `Unchanged` -> serve-cached-dirents path (a revalidated listing whose
//!   validator still matched serves the accumulated dirents, NOT an error),
//! - the rate-limited serve-stale path (serve the last-known listing rather than
//!   re-calling the provider and getting EAGAIN),
//! - the cache-populate half of a fresh provider listing (drop reserved-`@`
//!   provider entries, write the dirents record, append the `@next`/`@all`
//!   controls and the mount-root ignore files as renderer-neutral synthetic
//!   entries).
//!
//! A first-page browse listing (`cursor = None`) carries synthetic entries
//! (controls + ignore files) in `Listing::entries` with `EntryOrigin::Synthetic`;
//! an explicit-cursor continuation (`cursor = Some`) is a raw page drain with no
//! synthetic entries, so a renderer that flattens a dynamic dir into a finite
//! snapshot (NFS) drives the cursor forward over raw provider pages.

use crate::Runtime;
use crate::cache::{Record as CacheRecord, RecordKind};
use crate::ops::namespace::{DirEntry as ProviderEntry, DirListing as ProviderListing};
use crate::view::{CachedCursor, DirentRecord, DirentsPayload, EntryMeta};
use tracing::warn;

use super::error::{Result, TreeError};
use super::node::{Entry, Node, PaginationControl, Synthetic};
use super::synthetic;
use crate::tree_refs::TreeRef;
use crate::{RequestCtx, TreeNamespace};
use omnifs_api::events::CacheKind;

/// Opaque pagination cursor. Newtype over the substrate's `CachedCursor` so no
/// second cursor model is invented. Converted to/from provider cursors inside
/// namespace ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(pub CachedCursor);

/// Result of provider-list execution when the node is a provider directory. `exhaustive`
/// MUST survive the boundary: NFS turns a non-exhaustive dynamic dir into a
/// finite snapshot, and lookup stays the authoritative name oracle (readdir may
/// be non-exhaustive). `next_cursor` drives pagination through `Tree`.
///
/// `entries` contains both provider-projected children (with reserved-`@` names
/// already dropped) and host-synthesized entries tagged with `EntryOrigin::Synthetic`:
/// the `@next`/`@all` pagination controls when the listing carries a resume
/// cursor, and the mount-root ignore files at the mount root. Every renderer
/// materializes them identically. A continuation page (`cursor = Some`) returns
/// only provider entries.
#[derive(Debug, Clone)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub exhaustive: bool,
    pub next_cursor: Option<Cursor>,
}

/// `list()` either lists a provider directory or hands off a resolved backing
/// host-tree directory (a bind-mounted clone). A distinct variant so a tree-ref
/// dir can never be mistaken for a provider listing.
#[derive(Debug, Clone)]
pub enum ListOutcome {
    Listing(Listing),
    Host(TreeRef),
}

impl TreeNamespace {
    /// List a directory node. `cursor = None` starts a first-page browse listing
    /// (coalesced, cache-consulted, and carrying the host-synthesized control /
    /// ignore entries as synthetic `Entry` origins); `Some(cursor)` continues
    /// pagination as a raw page drain. Returns the internal listing outcome.
    pub(crate) async fn list(
        &self,
        node: &Node,
        cursor: Option<Cursor>,
        _ctx: &RequestCtx,
    ) -> Result<ListOutcome> {
        if let Some((tree_ref, _, _)) = node.host() {
            return Ok(ListOutcome::Host(tree_ref.clone()));
        }

        if self.is_mount_enumeration_root(node.mount(), node.path()) {
            let entries = self
                .mount_names()
                .into_iter()
                .map(|mount| Entry::provider(mount, EntryMeta::directory()))
                .collect();
            return Ok(ListOutcome::Listing(Listing {
                entries,
                exhaustive: true,
                next_cursor: None,
            }));
        }

        let runtime = self.runtime_for(node.mount())?;
        let path = node.path();

        // An explicit-cursor continuation is a raw page drain: no cache consult,
        // no synthetic entries, the direct provider paginated read.
        if let Some(cursor) = cursor {
            return self
                .list_continuation(&runtime, path, cursor)
                .await
                .map(ListOutcome::Listing);
        }

        // First-page browse listing. Drain pending invalidations so a stale mem
        // entry never satisfies the consult below, then serve an authoritative
        // cached listing if one exists.
        self.drain_invalidations(node.mount());
        if let Some(dirents) = consult_authoritative_listing(&runtime, path) {
            crate::inspector::cache_event(CacheKind::BrowseHit);
            return Ok(ListOutcome::Listing(listing_from_dirents(node, &dirents)));
        }

        self.list_via_provider(&runtime, node).await
    }

    /// Continuation page: echo the cursor to the provider, return raw entries.
    /// `Namespace::list_children` applies the continuation projection; this does
    /// no dirents write and appends no synthetic entries.
    async fn list_continuation(
        &self,
        runtime: &Runtime,
        path: &omnifs_core::path::Path,
        cursor: Cursor,
    ) -> Result<Listing> {
        crate::inspector::cache_event(CacheKind::BrowseMiss);
        let result = runtime.list_children(path, None, Some(cursor.0)).await?;
        match result {
            crate::ops::namespace::ListOutcome::Entries(listing) => Ok(Listing {
                entries: provider_entries(path, &listing.entries),
                exhaustive: listing.exhaustive && listing.next_cursor.is_none(),
                next_cursor: listing.next_cursor.map(Cursor),
            }),
            // A continuation that resolves to `unchanged` means the feed is
            // stable; treat it as exhausted with no further entries.
            crate::ops::namespace::ListOutcome::Unchanged => Ok(Listing {
                entries: Vec::new(),
                exhaustive: true,
                next_cursor: None,
            }),
            crate::ops::namespace::ListOutcome::Subtree(_) => Err(TreeError::internal(
                "list continuation resolved to a host-tree handoff",
            )),
        }
    }

    /// Cold first-page listing through the provider, owning the cache-populate
    /// half: revalidation validator echo + `Unchanged`-serve-cached, rate-limit
    /// serve-stale, the reserved-`@` drop, the dirents write, and the synthetic
    /// control / ignore append.
    async fn list_via_provider(&self, runtime: &Runtime, node: &Node) -> Result<ListOutcome> {
        let path = node.path();
        // A non-exhaustive cached dirents record may carry a listing validator
        // the provider can revalidate against; echo it so the provider can
        // answer `unchanged`.
        let cached_dirents = cached_dirents_for_revalidation(runtime, path);
        let cached_validator = cached_dirents.as_ref().and_then(|d| d.validator.clone());

        crate::inspector::cache_event(CacheKind::BrowseMiss);
        let result = runtime.list_children(path, cached_validator, None).await;

        match result {
            Ok(crate::ops::namespace::ListOutcome::Entries(listing)) => Ok(ListOutcome::Listing(
                snapshot_from_provider_listing(node, runtime, &listing),
            )),
            Ok(crate::ops::namespace::ListOutcome::Unchanged) => {
                // The cached validator still matched: serve the accumulated
                // dirents rather than erroring (the FUSE/NFS unchanged path).
                let Some(dirents) = cached_dirents else {
                    warn!(
                        path = path.as_str(),
                        "list_children returned unchanged with no cached listing"
                    );
                    return Err(TreeError::internal(
                        "list_children returned unchanged with no cached listing",
                    ));
                };
                Ok(ListOutcome::Listing(listing_from_dirents(node, &dirents)))
            },
            Ok(crate::ops::namespace::ListOutcome::Subtree(tref)) => {
                let dir = runtime
                    .tree_ref(tref)
                    .ok_or_else(|| TreeError::internal(format!("unresolved tree_ref {tref}")))?;
                Ok(ListOutcome::Host(dir))
            },
            Err(error) => {
                let rate_limited = error.is_provider_rate_limited();
                warn!(
                    path = path.as_str(),
                    error = %error,
                    "provider returned typed error for list_children"
                );
                // Serve stale so `ls` survives upstream throttling.
                if rate_limited && let Some(dirents) = cached_dirents {
                    return Ok(ListOutcome::Listing(listing_from_dirents(node, &dirents)));
                }
                Err(error.into())
            },
        }
    }
}

/// Build a `Listing` from a fresh provider listing, owning the cache-populate
/// half: drop reserved-`@` provider entries, write the dirents record (controls
/// included when paged, and never stripped back out even after the feed later
/// exhausts, so they survive a later cached serve and control-name resolution),
/// and surface the synthetic control / ignore entries.
fn snapshot_from_provider_listing(
    node: &Node,
    runtime: &Runtime,
    listing: &ProviderListing,
) -> Listing {
    let path = node.path();
    let mut dirent_records = Vec::with_capacity(listing.entries.len());
    for e in &listing.entries {
        // `@` is reserved for host control entries: a provider must never shadow
        // `@next`/`@all`. Skip with a warning.
        if synthetic::is_reserved_provider_leaf(&e.name) {
            warn!(
                name = e.name.as_str(),
                path = path.as_str(),
                "provider listing yielded a reserved '@'-prefixed entry; skipping"
            );
            continue;
        }
        if path.is_root() && synthetic::is_root_ignore_name(&e.name) {
            warn!(
                name = e.name.as_str(),
                path = path.as_str(),
                "provider listing yielded a host-owned root ignore entry; skipping"
            );
            continue;
        }
        dirent_records.push(DirentRecord {
            name: e.name.clone(),
            meta: e.meta.clone(),
        });
    }

    let next_cursor = listing.next_cursor.clone();
    let paginated = next_cursor.is_some();

    // The dirents record the host accumulates carries the `@next`/`@all` control
    // records when paged, so a later cached serve (and a control lookup) still
    // finds them. Build the persisted record before splitting out the
    // renderer-facing synthetic entries.
    let mut persisted = dirent_records.clone();
    if paginated {
        persisted.extend(synthetic::control_entries());
    }
    let dirents_payload = DirentsPayload {
        entries: persisted,
        // A paged listing is never exhaustive while a cursor remains.
        exhaustive: listing.exhaustive && next_cursor.is_none(),
        validator: listing.validator.clone(),
        next_cursor: next_cursor.clone(),
        paginated,
    };
    if let Some(encoded) = dirents_payload.serialize() {
        let record = CacheRecord::new(RecordKind::Dirents, encoded);
        runtime
            .resources
            .cache_put(path, RecordKind::Dirents, None, &record);
    }

    let mut entries: Vec<Entry> = dirent_records
        .into_iter()
        .map(|r| Entry::provider(r.name, r.meta))
        .collect();
    entries.extend(synthetic_entries_for(node, paginated));
    Listing {
        entries,
        exhaustive: dirents_payload.exhaustive,
        next_cursor: next_cursor.map(Cursor),
    }
}

/// The host-synthesized entries for a first-page browse listing: `@next`/`@all`
/// controls when the directory is paged, plus the mount-root ignore files at the
/// mount root.
fn synthetic_entries_for(node: &Node, paginated: bool) -> Vec<Entry> {
    let mut out = Vec::new();
    if paginated {
        out.extend(PaginationControl::entries());
    }
    if node.path().is_root() {
        out.extend(Synthetic::root_ignore_entries());
    }
    out
}

/// Provider-projected entries with reserved-`@` names dropped.
fn provider_entries(path: &omnifs_core::path::Path, entries: &[ProviderEntry]) -> Vec<Entry> {
    entries
        .iter()
        .filter(|e| {
            !(synthetic::is_reserved_provider_leaf(&e.name)
                || path.is_root() && synthetic::is_root_ignore_name(&e.name))
        })
        .map(|e| Entry::provider(e.name.clone(), e.meta.clone()))
        .collect()
}

/// Build a `Listing` from an accumulated/cached dirents record (the
/// serve-cached, `Unchanged`, and rate-limit-stale paths). The cached record
/// keeps the `@next`/`@all` control records for the directory's whole
/// paginated lifetime (`pagination.rs` never strips them, even at
/// exhaustion, so a name a consumer resolved from an earlier listing keeps
/// resolving); they are dropped from `entries` here and re-surfaced as
/// synthetic entries ONLY while `next_cursor` is still `Some`, so a FRESH
/// listing stops naming them the moment the feed exhausts even though the
/// persisted records outlive that moment.
fn listing_from_dirents(node: &Node, dirents: &DirentsPayload) -> Listing {
    let mut entries = Vec::with_capacity(dirents.entries.len());
    for record in &dirents.entries {
        if synthetic::is_reserved_provider_leaf(&record.name)
            || (node.path().is_root() && synthetic::is_root_ignore_name(&record.name))
        {
            continue;
        }
        entries.push(Entry::provider(record.name.clone(), record.meta.clone()));
    }

    let mut synthetic_entries = Vec::new();
    if dirents.next_cursor.is_some() {
        synthetic_entries.extend(PaginationControl::entries());
    }
    if node.path().is_root() {
        synthetic_entries.extend(Synthetic::root_ignore_entries());
    }
    entries.extend(synthetic_entries);

    Listing {
        entries,
        exhaustive: dirents.exhaustive,
        next_cursor: dirents.next_cursor.clone().map(Cursor),
    }
}

/// Serve a directory listing from cache only for an authoritative record: an
/// exhaustive record, or a host-accumulated paginated record (still paging, or
/// exhausted-but-complete). A plain non-exhaustive record is a partial
/// prefetched snapshot (e.g. from a Lookup that merged route-derived structural
/// children) and must NOT be returned as authoritative; the caller falls through
/// to the provider. Mirrors `Frontend::opendir_check_caches` + rate-limit
/// serve-stale.
fn consult_authoritative_listing(
    runtime: &Runtime,
    path: &omnifs_core::path::Path,
) -> Option<DirentsPayload> {
    let dirents = cached_dirents_for_revalidation(runtime, path);
    if dirents
        .as_ref()
        .is_some_and(DirentsPayload::is_authoritative_listing)
    {
        return dirents;
    }
    // Serve-stale-while-rate-limited: while the mount's window is open, serve the
    // last-known listing (even a non-authoritative prefix) rather than calling
    // the provider and getting EAGAIN.
    if runtime.rate_limited_until().is_some() {
        return dirents;
    }
    None
}

/// The cached dirents record for `path`, exhaustive or not, used to recover the
/// listing validator for revalidation and to serve an `unchanged` result.
/// Mirrors `Frontend::cached_dirents_for_revalidation`.
fn cached_dirents_for_revalidation(
    runtime: &Runtime,
    path: &omnifs_core::path::Path,
) -> Option<DirentsPayload> {
    let record = runtime
        .resources
        .cache_get(path, RecordKind::Dirents, None)?;
    DirentsPayload::deserialize(&record.payload)
}
