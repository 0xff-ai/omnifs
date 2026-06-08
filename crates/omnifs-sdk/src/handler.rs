//! Handler context, pagination cursor, subtree handles, and ranged-read session types.

use crate::cx::Cx;
use crate::error::Result;
use crate::file_attrs::FileAttrs;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

pub use crate::file_attrs::MAX_PROJECTED_BYTES;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cursor {
    Opaque(String),
    Page(u32),
}

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

    /// The host-supplied pagination cursor for a `list-children` call. `None`
    /// on the first page or for non-list intents; the handler resumes from it.
    pub fn cursor(&self) -> Option<&Cursor> {
        match &self.intent {
            DirIntent::List { cursor } => cursor.as_ref(),
            _ => None,
        }
    }
}

impl<S> std::ops::Deref for DirCx<S> {
    type Target = Cx<S>;

    fn deref(&self) -> &Cx<S> {
        &self.cx
    }
}

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

pub trait RangeReader {
    fn read_chunk(&self, offset: u64, length: u32) -> BoxFuture<'_, FileChunk>;
}

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
    fn read_chunk(&self, offset: u64, length: u32) -> BoxFuture<'_, FileChunk> {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeRef {
    pub tree_ref: u64,
}

impl TreeRef {
    pub fn new(tree_ref: u64) -> Self {
        Self { tree_ref }
    }
}
