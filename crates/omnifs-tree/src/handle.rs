//! Ranged read handle and the `Tree::open` body.
//!
//! Slice 5 work; signed now so the surface is stable. `Tree::open` and
//! `RangedHandle::read` are `todo!()`-bodied; `RangedHandle::close` is a
//! one-liner over `Runtime::call_close_file` and is implemented.

use std::sync::Arc;

use omnifs_core::path::Path;
use omnifs_core::view::FileAttrsCache;
use omnifs_host::Runtime;

use crate::error::Result;
use crate::node::Node;
use crate::read::Chunk;
use crate::{RequestCtx, Tree};

/// Runtime-owned ranged read handle for `Deferred(Ranged)` files. Holds an
/// `Arc<Runtime>` so it is self-contained across renderer calls. The renderer
/// binds it to its own kernel handle (FUSE fh / NFS stateid) and owns ONLY that
/// mapping + lease; the provider handle (u64) and chunk validation are neutral.
pub struct RangedHandle {
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) path: Path,
    pub(crate) provider_handle: u64,
    pub(crate) attrs: FileAttrsCache,
}

impl RangedHandle {
    pub fn attrs(&self) -> &FileAttrsCache {
        &self.attrs
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn provider_handle(&self) -> u64 {
        self.provider_handle
    }

    /// Drive provider `read_chunk`; learn exact size on EOF. SLICE 5.
    // Async surface is intentional (slice 5 body awaits Namespace::read_chunk).
    #[allow(clippy::unused_async)]
    pub async fn read(&self, _offset: u64, _length: u32) -> Result<Chunk> {
        todo!("slice 5: Namespace::read_chunk + learned-EOF size")
    }

    /// Release the provider handle via `Runtime::call_close_file`. Sync: the
    /// pass-through is sync. The renderer calls this when its fh/stateid is
    /// released or invalidated.
    pub fn close(self) -> Result<()> {
        self.runtime
            .call_close_file(self.provider_handle)
            .map_err(Into::into)
    }
}

impl Tree {
    /// Open a `Deferred(Ranged)` file. SLICE 1: `todo!()`. Final body: dispatch
    /// `Namespace::open_file`, validate/derive `opened_file_attrs`, return a
    /// runtime-owned `RangedHandle` the renderer binds to its kernel handle.
    // Async surface is intentional (slice 5 body awaits Namespace::open_file).
    #[allow(clippy::unused_async)]
    pub async fn open(&self, _node: &Node, _ctx: &RequestCtx) -> Result<RangedHandle> {
        todo!("slice 5: open_file dispatch + opened_file_attrs derivation")
    }
}
