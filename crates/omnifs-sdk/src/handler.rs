//! Handler context, pagination cursor, subtree handles, and ranged-read session types.

use crate::cx::Cx;
use crate::error::Result;
use crate::file_attrs::FileAttrs;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + 'a>>;

/// A pagination cursor for non-exhaustive listings: an upstream-issued
/// opaque token, or a plain page number. Return the next cursor with a
/// paged listing and the host echoes it back on the continuation call;
/// the provider holds no pagination state between calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cursor {
    Opaque(String),
    Page(u32),
}

/// The operation a directory handler is being invoked for.
///
/// One dir handler serves lookup and list (the same route knows the same
/// names), so check the intent when the answers should differ in cost: a
/// `Lookup` asks about one named child and should not trigger a full
/// upstream enumeration when a point query exists; a `List` enumerates.
/// Returning the full listing for both is correct, just potentially
/// wasteful.
///
/// ```ignore
/// match cx.intent() {
///     DirIntent::Lookup { child } => {
///         // point query: does `child` exist?
///     },
///     _ => {
///         // enumerate
///     },
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirIntent {
    Lookup { child: String },
    List { cursor: Option<Cursor> },
    ReadFile { name: String },
}

/// Directory handler context: a `Cx<S>` paired with the request intent.
///
/// Dir handlers serve three operations (lookup, list, read-file);
/// `DirCx` carries which one the host asked for. Derefs to `Cx<S>` so all
/// the usual context methods (`.http()`, `.git()`, `.state()`) work directly.
pub struct DirCx<S = ()> {
    cx: Cx<S>,
    intent: DirIntent,
}

impl<S> DirCx<S> {
    pub fn new(cx: Cx<S>, intent: DirIntent) -> Self {
        Self { cx, intent }
    }

    pub fn intent(&self) -> &DirIntent {
        &self.intent
    }

    /// The host-supplied pagination cursor for a `list-children` call.
    /// `None` on the first page or for non-list intents; the handler
    /// resumes from it.
    pub fn cursor(&self) -> Option<&Cursor> {
        match &self.intent {
            DirIntent::List { cursor } => cursor.as_ref(),
            _ => None,
        }
    }

    /// The `Cursor::Page` value, or `default` when no page cursor is present.
    pub fn page_cursor(&self, default: u32) -> u32 {
        match self.cursor() {
            Some(Cursor::Page(n)) => *n,
            _ => default,
        }
    }
}

impl<S> std::ops::Deref for DirCx<S> {
    type Target = Cx<S>;

    fn deref(&self) -> &Cx<S> {
        &self.cx
    }
}

/// One ranged-read answer: the bytes plus an `eof` flag meaning "this
/// chunk reaches the end of the content as of this read".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChunk {
    pub content: Vec<u8>,
    pub eof: bool,
}

impl FileChunk {
    pub fn new(content: impl Into<Vec<u8>>, eof: bool) -> Self {
        Self {
            content: content.into(),
            eof,
        }
    }
}

impl From<FileChunk> for omnifs_wit::provider::types::ReadChunkResult {
    fn from(chunk: FileChunk) -> Self {
        Self {
            content: chunk.content,
            eof: chunk.eof,
        }
    }
}

/// Byte server for an open ranged-read session (`open-file` /
/// `read-chunk` / `close-file`).
///
/// The contract: return up to `length` bytes starting at `offset`, with
/// `eof = true` when the chunk reaches the current end of content. An
/// `offset` at or past the end yields an empty chunk with `eof = true`,
/// not an error. Short chunks are legal; the host keeps reading. This is
/// the only handler shape allowed to serve `Stability::Live` content,
/// and the reader may be polled repeatedly on one open handle (`tail -f`),
/// so each call should observe current bytes rather than a snapshot when
/// volatility is the point.
pub trait RangeReader {
    /// Serve a chunk. `cx` is the state-erased context of the running
    /// read-chunk operation, so a reader that fetches ranges on demand can
    /// `cx.http()…send().await` and the WIT async host import runs under this
    /// operation. In-memory readers ignore it.
    fn read_chunk<'a>(
        &'a self,
        cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk>;
}

/// A [`RangeReader`] over a fully buffered in-memory payload.
///
/// Fine when the content is small or was already materialized anyway
/// (a rendered report, a decoded field). For large remote content it
/// defeats the purpose of ranged reads, since the whole payload is held
/// in guest memory up front; serve those through blob handles or a reader
/// that fetches ranges on demand.
#[derive(Clone, Debug)]
pub struct MemoryRangeReader {
    bytes: Rc<Vec<u8>>,
}

impl MemoryRangeReader {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: Rc::new(bytes.into()),
        }
    }
}

impl RangeReader for MemoryRangeReader {
    fn read_chunk<'a>(
        &'a self,
        _cx: &'a Cx<()>,
        offset: u64,
        length: u32,
    ) -> BoxFuture<'a, FileChunk> {
        Box::pin(async move {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= self.bytes.len() {
                return Ok(FileChunk::new(Vec::new(), true));
            }
            let end = start.saturating_add(length as usize).min(self.bytes.len());
            Ok(FileChunk::new(
                self.bytes[start..end].to_vec(),
                end == self.bytes.len(),
            ))
        })
    }
}

/// An open ranged-read session: the file's attrs plus the reader that
/// serves its chunks. Produced by router `open_file` dispatch from a
/// ranged projection; held until `close-file`.
#[derive(Clone)]
pub struct OpenedFile {
    pub attrs: FileAttrs,
    pub reader: Rc<dyn RangeReader>,
}

impl OpenedFile {
    pub fn new(attrs: FileAttrs, reader: Rc<dyn RangeReader>) -> Self {
        Self { attrs, reader }
    }
}

/// The subtree handoff token a `treeref` handler returns: a host-issued
/// tree handle from a git-open callout that the host
/// resolves to a bind-mounted directory. Provider dispatch stops at the
/// handoff point; paths below it never reach the provider.
///
/// ```ignore
/// async fn tree(cx: Cx, key: RepoKey) -> Result<TreeRef> {
///     let opened = cx
///         .git()
///         .open_repo("git@github.com:o/r.git".to_string())
///         .send()
///         .await?;
///     Ok(TreeRef::new(opened.tree))
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeRef {
    pub tree_ref: u64,
}

impl TreeRef {
    pub fn new(tree_ref: u64) -> Self {
        Self { tree_ref }
    }
}
