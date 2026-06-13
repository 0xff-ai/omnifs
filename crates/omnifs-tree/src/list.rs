//! Directory listing types and the `Tree::list` body.

use std::path::PathBuf;

use omnifs_core::view::CachedCursor;
use omnifs_host::wit_protocol::{
    cached_cursor_from_wit, cached_cursor_to_wit, entry_meta_from_kind,
};
use omnifs_wit::provider::types as wit_types;

use crate::error::{Result, TreeError};
use crate::node::{Entry, Node};
use crate::{RequestCtx, Tree};

/// Opaque pagination cursor. Newtype over the substrate's `CachedCursor` so no
/// second cursor model is invented. Converted to/from `wit_types::Cursor`
/// inside `Tree` via `omnifs_host::wit_protocol::cached_cursor_{from,to}_wit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor(pub CachedCursor);

/// Result of `Tree::list` when the node is a provider directory. `exhaustive`
/// MUST survive the boundary: NFS turns a non-exhaustive dynamic dir into a
/// finite snapshot, and lookup stays the authoritative name oracle (readdir may
/// be non-exhaustive). `next_cursor` drives pagination through `Tree`.
#[derive(Debug, Clone)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub exhaustive: bool,
    pub next_cursor: Option<Cursor>,
}

/// `list()` either lists a provider directory or hands off a resolved backing
/// subtree dir (a bind-mounted clone/archive). A distinct variant so a treeref
/// dir can never be mistaken for a provider listing.
#[derive(Debug, Clone)]
pub enum ListOutcome {
    Listing(Listing),
    Subtree(PathBuf),
}

impl Tree {
    /// List a directory node. cursor=None starts a listing (coalesced),
    /// Some(cursor) continues pagination. Returns `ListOutcome::Listing` or
    /// `ListOutcome::Subtree(backing_dir)`. The @next/@all control-file
    /// presentation is a FUSE layer ON TOP of this, not part of `Listing`.
    pub async fn list(
        &self,
        node: &Node,
        cursor: Option<Cursor>,
        ctx: &RequestCtx,
    ) -> Result<ListOutcome> {
        if let crate::node::Backing::Subtree(dir) = node.backing() {
            return Ok(ListOutcome::Subtree(dir.clone()));
        }

        let runtime = self.runtime_for(node.mount())?;
        let wit_cursor = cursor.map(|c| cached_cursor_to_wit(c.0));
        let result = runtime
            .namespace()
            .list_children(node.path().as_str(), None, wit_cursor, ctx.trace)
            .await?;

        match result {
            wit_types::ListChildrenResult::Entries(listing) => {
                let entries = listing
                    .entries
                    .iter()
                    .map(|e| Entry {
                        name: e.name.clone(),
                        meta: entry_meta_from_kind(&e.kind),
                    })
                    .collect();
                Ok(ListOutcome::Listing(Listing {
                    entries,
                    exhaustive: listing.exhaustive,
                    next_cursor: listing
                        .next_cursor
                        .map(|c| Cursor(cached_cursor_from_wit(c))),
                }))
            },
            // The tracer drives cursor=None against a cold cache, so a provider
            // listing always returns Entries. Unchanged means the cached
            // validator still matched; rebuilding the Listing from cached
            // dirents is a follow-up beat (it needs the cached DirentsPayload
            // round trip). Slice 1 surfaces it as Internal rather than
            // silently returning an empty listing.
            wit_types::ListChildrenResult::Unchanged => Err(TreeError::internal(
                "list: provider returned Unchanged; cached-dirents rebuild is a follow-up beat",
            )),
            wit_types::ListChildrenResult::Subtree(tref) => {
                let dir = runtime
                    .resolve_tree_ref(tref)
                    .ok_or_else(|| TreeError::internal(format!("unresolved tree_ref {tref}")))?;
                Ok(ListOutcome::Subtree(dir))
            },
        }
    }
}
