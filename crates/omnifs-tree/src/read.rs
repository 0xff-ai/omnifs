//! Whole-file read result types and the `Tree::read` body.
//!
//! SLICE 1: `Tree::read` is `todo!()`-bodied but fully signed. The final body
//! is the read cache cascade + op_gen fence + canonical-hybrid + learned-size
//! (slice 3). This module is also the future home of the byte-source resolver
//! (FUSE `read_helpers.rs::resolve_read_payload` and the learned-size helpers).

use std::path::PathBuf;

use omnifs_core::view::FileAttrsCache;

use crate::error::Result;
use crate::node::Node;
use crate::{RequestCtx, Tree};

/// Result of `Tree::read`. A two-arm shape so a treeref-backed node (read via
/// renderer std::fs passthrough over a real dir) can never be confused with
/// resolved provider bytes. `Bytes.attrs` is the POST-read learned attrs (exact
/// size promoted from the bytes) the renderer applies to st_size / the NFSv4
/// change attribute; `content_type` echoes the rendered representation type.
#[derive(Debug, Clone)]
pub enum ReadResult {
    Bytes {
        data: Vec<u8>,
        attrs: FileAttrsCache,
        content_type: Option<String>,
    },
    Backing(PathBuf),
}

/// One ranged chunk from a `RangedHandle`. `learned_attrs` is `Some` on an
/// EOF-short read when an exact size was learned, so the renderer promotes
/// st_size (preserves today's `learned_ranged_eof_attrs` behavior).
#[derive(Debug, Clone)]
pub struct Chunk {
    pub bytes: Vec<u8>,
    pub eof: bool,
    pub learned_attrs: Option<FileAttrsCache>,
}

impl Tree {
    /// Whole-file read. SLICE 1: `todo!()`. Final body: cache-consult cascade
    /// (exact-0 short-circuit, durable view hit keyed by durable_cache_aux),
    /// backing-fs read for `Backing::Subtree` (-> `ReadResult::Backing`), else
    /// capture op_gen = `Runtime::current_generation()` BEFORE awaiting
    /// `Namespace::read_file`, resolve the byte source (Inline/Blob via
    /// `read_blob_full` / Canonical via `canonical_bytes_for`, NEVER copied
    /// into the view cache), validate bytes vs attrs, learn size, and populate
    /// the durable view only if NOT `Runtime::write_fenced(path, op_gen)`.
    // Async surface is intentional (slice 3 body awaits Namespace::read_file);
    // the stub has no await yet.
    #[allow(clippy::unused_async)]
    pub async fn read(&self, _node: &Node, _ctx: &RequestCtx) -> Result<ReadResult> {
        todo!("slice 3: read cache cascade + op_gen fence + canonical hybrid + learned size")
    }
}
