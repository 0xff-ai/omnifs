use crate::browse::{
    Effects, Entry as BrowseEntry, EntryKind as BrowseEntryKind, List as BrowseList,
    Listing as BrowseListing, Lookup as BrowseLookup,
};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::{FileAttrs, FileProj, ReadMode, Size, Stability};
use omnifs_mount_schema::{PathPattern, PathSegment, split_path};
use std::any::Any;
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
pub struct DirCx<S> {
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
}

impl<S> std::ops::Deref for DirCx<S> {
    type Target = Cx<S>;

    fn deref(&self) -> &Cx<S> {
        &self.cx
    }
}

/// Context for handlers declared inside a `#[subtree] impl B { ... }`
/// block. Wraps the underlying `Cx<S>` and exposes the bindings `B`
/// captured at the bind site.
///
/// For dir-shaped handlers in a subtree, the request intent is exposed
/// via `intent()`; for file-shaped handlers, it's `None`. Methods on
/// `Cx<S>` (`.http()`, `.git()`, `.state()`) are available through
/// `Deref`.
pub struct BindCtx<'a, S, B> {
    cx: &'a Cx<S>,
    bindings: &'a B,
    intent: Option<DirIntent>,
}

impl<'a, S, B> BindCtx<'a, S, B> {
    pub fn new(cx: &'a Cx<S>, bindings: &'a B, intent: Option<DirIntent>) -> Self {
        Self {
            cx,
            bindings,
            intent,
        }
    }

    pub fn bindings(&self) -> &B {
        self.bindings
    }

    pub fn intent(&self) -> Option<&DirIntent> {
        self.intent.as_ref()
    }
}

impl<S, B> std::ops::Deref for BindCtx<'_, S, B> {
    type Target = Cx<S>;

    fn deref(&self) -> &Cx<S> {
        self.cx
    }
}

/// A typed subtree handler. The `#[subtree] impl B { ... }` macro
/// generates an implementation; provider authors do not implement
/// this trait directly.
///
/// No `Send + Sync` bound: the runtime is single-threaded (`Rc`-based).
pub trait Handler<S> {
    fn lookup_child<'a>(
        &'a self,
        cx: &'a Cx<S>,
        parent_path: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, crate::browse::Lookup>;

    fn list_children<'a>(
        &'a self,
        cx: &'a Cx<S>,
        path: &'a str,
    ) -> BoxFuture<'a, crate::browse::List>;

    fn read_file<'a>(
        &'a self,
        cx: &'a Cx<S>,
        path: &'a str,
    ) -> BoxFuture<'a, crate::browse::FileContent>;

    fn open_file<'a>(&'a self, cx: &'a Cx<S>, path: &'a str) -> BoxFuture<'a, OpenedFile>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageStatus {
    Exhaustive,
    More(Cursor),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub size: u64,
}

impl FileStat {
    pub fn exact(size: u64) -> Self {
        Self { size }
    }
}

#[derive(Clone, Debug)]
enum ProjectionEntry {
    Directory { name: String },
    File { name: String, file: FileProj },
}

impl ProjectionEntry {
    fn name(&self) -> &str {
        match self {
            Self::Directory { name } | Self::File { name, .. } => name,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Projection {
    entries: Vec<ProjectionEntry>,
    page: Option<PageStatus>,
    effects: Effects,
    errors: Vec<String>,
}

impl Projection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dir(&mut self, name: impl Into<String>) {
        let _ = self.push_dir(name.into());
    }

    pub fn file(&mut self, name: impl Into<String>, file: FileProj) {
        let _ = self.push_file(name.into(), file);
    }

    pub fn deferred_file(&mut self, name: impl Into<String>) {
        self.file(
            name,
            FileProj::deferred(Size::Unknown, ReadMode::Full, Stability::Immutable),
        );
    }

    pub fn file_with_stat(&mut self, name: impl Into<String>, stat: FileStat) {
        self.file(
            name,
            FileProj::deferred(Size::Exact(stat.size), ReadMode::Full, Stability::Immutable),
        );
    }

    pub fn file_with_content(&mut self, name: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.file(name, FileProj::inline(bytes, Stability::Immutable, None));
    }

    pub fn file_with_content_attrs(
        &mut self,
        name: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
        stability: Stability,
        version: Option<crate::file_attrs::VersionToken>,
    ) {
        let bytes = bytes.into();
        self.file(name, FileProj::inline(bytes, stability, version));
    }

    pub fn page(&mut self, status: PageStatus) {
        self.page = Some(status);
    }

    /// Project file content for `path` when the operation return is
    /// accepted. Empty paths are dropped silently. Empty content is
    /// preserved as a valid file.
    pub fn proj(&mut self, path: impl Into<String>, content: impl Into<Vec<u8>>) {
        let path = path.into();
        let content = content.into();
        self.proj_file(path, FileProj::inline(content, Stability::Immutable, None));
    }

    pub fn proj_file(&mut self, path: impl Into<String>, file: FileProj) {
        let path = path.into();
        if let Err(error) = self.effects.project_file(path, file) {
            self.record_error(error.message().to_string());
        }
    }

    /// Project a batch of inline file contents.
    pub fn proj_many<I, P, B>(&mut self, files: I)
    where
        I: IntoIterator<Item = (P, B)>,
        P: Into<String>,
        B: Into<Vec<u8>>,
    {
        for (path, content) in files {
            self.proj(path, content);
        }
    }

    /// Project directory metadata for `path`.
    pub fn proj_dir(&mut self, path: impl Into<String>) {
        if let Err(error) = self.effects.project_dir(path) {
            self.record_error(error.message().to_string());
        }
    }

    pub fn into_error(self) -> Option<String> {
        if self.errors.is_empty() {
            None
        } else {
            Some(self.errors.join("; "))
        }
    }

    fn push_dir(&mut self, name: String) -> Result<()> {
        self.validate_child_name(&name)?;
        self.entries.push(ProjectionEntry::Directory { name });
        Ok(())
    }

    fn push_file(&mut self, name: String, file: FileProj) -> Result<()> {
        self.validate_child_name(&name)?;
        file.validate()
            .map_err(|error| ProviderError::invalid_input(error.message().to_string()))?;
        self.entries.push(ProjectionEntry::File { name, file });
        Ok(())
    }

    fn validate_child_name(&mut self, name: &str) -> Result<()> {
        if !is_valid_rel_segment(name) {
            return self.reject(format!("invalid child name {name:?}"));
        }
        if self.entries.iter().any(|entry| entry.name() == name) {
            return self.reject(format!("duplicate child name {name:?}"));
        }
        Ok(())
    }

    fn record_error(&mut self, message: String) {
        self.errors.push(message);
    }

    fn reject<T>(&mut self, message: String) -> Result<T> {
        self.record_error(message.clone());
        Err(ProviderError::invalid_input(message))
    }
}

#[derive(Clone)]
pub enum FileContent {
    Bytes(Vec<u8>),
    BytesWithAttrs {
        attrs: FileAttrs,
        bytes: Vec<u8>,
    },
    Stream(StreamHandle),
    Range {
        file: FileProj,
        reader: Rc<dyn RangeReader>,
    },
}

impl FileContent {
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(bytes.into())
    }

    pub fn bytes_with_attrs(attrs: FileAttrs, bytes: impl Into<Vec<u8>>) -> Self {
        Self::BytesWithAttrs {
            attrs,
            bytes: bytes.into(),
        }
    }

    pub fn stream(handle: StreamHandle) -> Self {
        Self::Stream(handle)
    }

    pub fn ranged(attrs: FileAttrs, reader: impl RangeReader + 'static) -> Self {
        Self::Range {
            file: FileProj {
                attrs,
                bytes: crate::file_attrs::ProjBytes::Deferred {
                    read: ReadMode::Ranged,
                },
            },
            reader: Rc::new(reader),
        }
    }

    pub fn range_bytes(attrs: FileAttrs, bytes: impl Into<Vec<u8>>) -> Self {
        Self::ranged(attrs, MemoryRangeReader::new(bytes))
    }
}

impl std::fmt::Debug for FileContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(bytes) => f.debug_tuple("Bytes").field(bytes).finish(),
            Self::BytesWithAttrs { attrs, bytes } => f
                .debug_struct("BytesWithAttrs")
                .field("attrs", attrs)
                .field("bytes", bytes)
                .finish(),
            Self::Stream(handle) => f.debug_tuple("Stream").field(handle).finish(),
            Self::Range { file, .. } => f.debug_struct("Range").field("file", file).finish(),
        }
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

impl From<FileChunk> for crate::omnifs::provider::types::ReadChunkResult {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamHandle {
    pub id: u64,
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

#[derive(Clone)]
struct RouteDecl {
    template: &'static str,
    pattern: PathPattern,
}

type ParseFn = fn(&str) -> Option<Box<dyn Any>>;
type DirCallFn<S> = for<'a> fn(&'a Cx<S>, Box<dyn Any>, DirIntent) -> BoxFuture<'a, Projection>;
type FileCallFn<S> = for<'a> fn(&'a Cx<S>, Box<dyn Any>) -> BoxFuture<'a, FileContent>;
type TreeRefCallFn<S> = for<'a> fn(&'a Cx<S>, Box<dyn Any>) -> BoxFuture<'a, TreeRef>;

/// Builds a typed subtree handler from prefix captures parsed at the
/// bind site. The returned `Box<dyn Handler<S>>` owns its bindings
/// and routes subsequent path segments through its own per-type
/// registry.
#[allow(dead_code)] // wired through MountRegistry dispatch in Phase 1B-ii.
type BindCallFn<S> = for<'a> fn(&'a Cx<S>, Box<dyn Any>) -> BoxFuture<'a, Box<dyn Handler<S>>>;

/// Per-route call dispatcher inside a `#[subtree] impl B { ... }`.
/// The `&'a B` is the bindings carried over from the bind site;
/// the user-facing handler signature receives a `BindCtx<'_, S, B>`
/// constructed from `(cx, bindings, intent)`.
type SubtreeDirCallFn<S, B> =
    for<'a> fn(&'a Cx<S>, &'a B, Box<dyn Any>, DirIntent) -> BoxFuture<'a, Projection>;
type SubtreeFileCallFn<S, B> =
    for<'a> fn(&'a Cx<S>, &'a B, Box<dyn Any>) -> BoxFuture<'a, FileContent>;

struct DirHandlerRegistration<S> {
    decl: RouteDecl,
    parse: ParseFn,
    call: DirCallFn<S>,
}

struct FileHandlerRegistration<S> {
    decl: RouteDecl,
    parse: ParseFn,
    call: FileCallFn<S>,
}

struct TreeRefHandlerRegistration<S> {
    decl: RouteDecl,
    parse: ParseFn,
    call: TreeRefCallFn<S>,
}

#[allow(dead_code)] // fields read once dispatch hook lands in Phase 1B-ii.
struct BindRegistration<S> {
    decl: RouteDecl,
    parse: ParseFn,
    call: BindCallFn<S>,
}

pub struct MountRegistry<S> {
    dirs: Vec<DirHandlerRegistration<S>>,
    files: Vec<FileHandlerRegistration<S>>,
    treerefs: Vec<TreeRefHandlerRegistration<S>>,
    binds: Vec<BindRegistration<S>>,
}

impl<S> Default for MountRegistry<S> {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            files: Vec::new(),
            treerefs: Vec::new(),
            binds: Vec::new(),
        }
    }
}

impl<S> MountRegistry<S> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_dir(
        &mut self,
        template: &'static str,
        parse: ParseFn,
        call: DirCallFn<S>,
    ) -> Result<()> {
        self.dirs.push(DirHandlerRegistration {
            decl: RouteDecl {
                template,
                pattern: PathPattern::parse(template)
                    .map_err(|error| ProviderError::invalid_input(error.message().to_string()))?,
            },
            parse,
            call,
        });
        Ok(())
    }

    pub fn add_file(
        &mut self,
        template: &'static str,
        parse: ParseFn,
        call: FileCallFn<S>,
    ) -> Result<()> {
        self.files.push(FileHandlerRegistration {
            decl: RouteDecl {
                template,
                pattern: PathPattern::parse(template)
                    .map_err(|error| ProviderError::invalid_input(error.message().to_string()))?,
            },
            parse,
            call,
        });
        Ok(())
    }

    pub fn add_treeref(
        &mut self,
        template: &'static str,
        parse: ParseFn,
        call: TreeRefCallFn<S>,
    ) -> Result<()> {
        self.treerefs.push(TreeRefHandlerRegistration {
            decl: RouteDecl {
                template,
                pattern: PathPattern::parse(template)
                    .map_err(|error| ProviderError::invalid_input(error.message().to_string()))?,
            },
            parse,
            call,
        });
        Ok(())
    }

    /// Register a bind: when a request path has `template` as its
    /// (longest) prefix, the dispatcher invokes `call` to construct the
    /// typed subtree handler, then routes the remaining suffix through
    /// the handler's own `#[subtree] impl` registry.
    pub fn add_bind(
        &mut self,
        template: &'static str,
        parse: ParseFn,
        call: BindCallFn<S>,
    ) -> Result<()> {
        self.binds.push(BindRegistration {
            decl: RouteDecl {
                template,
                pattern: PathPattern::parse(template)
                    .map_err(|error| ProviderError::invalid_input(error.message().to_string()))?,
            },
            parse,
            call,
        });
        Ok(())
    }

    pub fn validate(&mut self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::<&'static str>::new();
        for decl in self
            .dirs
            .iter()
            .map(|entry| &entry.decl)
            .chain(self.files.iter().map(|entry| &entry.decl))
            .chain(self.treerefs.iter().map(|entry| &entry.decl))
            .chain(self.binds.iter().map(|entry| &entry.decl))
        {
            if !seen.insert(decl.template) {
                return Err(ProviderError::invalid_input(format!(
                    "duplicate handler declared for {}",
                    decl.template
                )));
            }
        }
        let mut static_children =
            std::collections::BTreeMap::<(String, &'static str), &'static str>::new();
        for decl in self
            .dirs
            .iter()
            .map(|entry| &entry.decl)
            .chain(self.files.iter().map(|entry| &entry.decl))
            .chain(self.treerefs.iter().map(|entry| &entry.decl))
            .chain(self.binds.iter().map(|entry| &entry.decl))
        {
            let Some(child) = decl.pattern.static_child() else {
                continue;
            };
            let key = (decl.pattern.parent_signature(), child);
            if let Some(existing) = static_children.insert(key.clone(), decl.template) {
                return Err(ProviderError::invalid_input(format!(
                    "static child conflict for {} and {}",
                    existing, decl.template
                )));
            }
        }
        validate_ambiguous_routes(&self.dirs, "dir")?;
        validate_ambiguous_routes(&self.files, "file")?;
        validate_ambiguous_routes(&self.treerefs, "treeref")?;
        validate_ambiguous_routes(&self.binds, "bind")?;
        // Exclusivity: a bind prefix owns all descendants. Reject any
        // normal route whose template is a strict descendant of any
        // bind template.
        for bind in &self.binds {
            let bind_segments = bind.decl.pattern.segments().len();
            for decl in self
                .dirs
                .iter()
                .map(|entry| &entry.decl)
                .chain(self.files.iter().map(|entry| &entry.decl))
                .chain(self.treerefs.iter().map(|entry| &entry.decl))
            {
                if decl.pattern.segments().len() > bind_segments
                    && pattern_starts_with(&decl.pattern, &bind.decl.pattern)
                {
                    return Err(ProviderError::invalid_input(format!(
                        "route {} is a descendant of bind {} — bind prefixes own all descendants; declare deeper routes inside the subtree's #[subtree] impl",
                        decl.template, bind.decl.template
                    )));
                }
            }
        }
        // Bind dispatch picks the longest-prefix match. Sort once here
        // so the per-request matcher iterates without re-sorting.
        self.binds
            .sort_by_key(|h| std::cmp::Reverse(h.decl.pattern.segments().len()));
        Ok(())
    }

    pub async fn lookup_child(
        &self,
        cx: &Cx<S>,
        parent_path: &str,
        name: &str,
    ) -> Result<BrowseLookup> {
        debug_assert!(
            parent_path.is_empty() || parent_path.starts_with('/'),
            "lookup_child expects an absolute parent path"
        );
        let parent_abs = to_absolute_path(parent_path);
        let child_abs = join_absolute_path(&parent_abs, name);

        // Bind prefix matches the path being looked up exactly: the
        // bind entry itself is a directory; report it as such.
        //
        // `exhaustive(false)` is load-bearing: without it, the host's
        // lookup-side cache treats the bare `Lookup::entry` as "the
        // bind has no children" and writes an exhaustive empty Dirents
        // at this path. A subsequent readdir then short-circuits on
        // that cache and never invokes the subtree's `list_children`.
        if self
            .binds
            .iter()
            .any(|h| h.decl.pattern.matches_path(&child_abs))
        {
            return Ok(BrowseLookup::entry(BrowseEntry::dir(name)).exhaustive(false));
        }

        // Bind prefix is a strict ancestor of the path: dispatch
        // through the typed handler with the relative suffix.
        if let Some((route, parsed, suffix)) = self.match_bind_prefix(&parent_abs) {
            let handler = (route.call)(cx, parsed).await?;
            return handler.lookup_child(cx, &suffix, name).await;
        }

        if let Some((route, parsed)) = self.match_treeref(&child_abs) {
            let tree_ref = (route.call)(cx, parsed).await?.tree_ref;
            return Ok(BrowseLookup::subtree(child_abs, tree_ref));
        }

        if let Some((route, parsed)) = self.match_dir(&child_abs) {
            // Exact dir lookups can warm the looked-up directory's adjacent cache shape.
            let projection = (route.call)(cx, parsed, DirIntent::List { cursor: None }).await?;
            return projection_exact_lookup(&projection, &child_abs, BrowseEntry::dir(name), self);
        }

        if let Some((route, parsed)) = self.match_dir(&parent_abs) {
            let projection = (route.call)(
                cx,
                parsed,
                DirIntent::Lookup {
                    child: name.to_string(),
                },
            )
            .await?;
            return projection_lookup(
                &projection,
                &parent_abs,
                name,
                self.exact_entry_for_path(&child_abs),
                self,
            );
        }

        if let Some(target) = self.exact_entry_for_path(&child_abs) {
            let siblings = self.static_entries_for_parent(&parent_abs);
            let exhaustive = !self.has_capture_child_under(&parent_abs);
            return Ok(BrowseLookup::entry(target)
                .with_siblings(siblings)
                .exhaustive(exhaustive));
        }

        Ok(BrowseLookup::not_found())
    }

    pub async fn list_children(&self, cx: &Cx<S>, path: &str) -> Result<BrowseList> {
        debug_assert!(
            path.is_empty() || path.starts_with('/'),
            "list_children expects an absolute path"
        );
        let abs = to_absolute_path(path);

        // Bind prefix matches the listed path exactly or as ancestor:
        // dispatch through the typed handler with the relative path.
        if let Some((route, parsed, suffix)) = self.match_bind_at_or_below(&abs) {
            let handler = (route.call)(cx, parsed).await?;
            return handler.list_children(cx, &suffix).await;
        }

        if let Some((route, parsed)) = self.match_treeref(&abs) {
            let tree_ref = (route.call)(cx, parsed).await?.tree_ref;
            return Ok(BrowseList::subtree(abs, tree_ref));
        }

        let static_entries = self.static_entries_for_parent(&abs);
        if let Some((route, parsed)) = self.match_dir(&abs) {
            let projection = (route.call)(cx, parsed, DirIntent::List { cursor: None }).await?;
            return projection_listing(&projection, static_entries).map(BrowseList::entries);
        }

        if self.is_implicit_prefix_dir(&abs) {
            let listing = if self.has_capture_child_under(&abs) {
                BrowseListing::partial(static_entries)
            } else {
                BrowseListing::complete(static_entries)
            };
            return Ok(BrowseList::entries(listing));
        }

        if self.match_file(&abs).is_some() {
            return Err(ProviderError::not_a_directory(format!("{path} is a file")));
        }

        Err(ProviderError::not_found(format!("path not found: {path}")))
    }

    pub async fn read_file(&self, cx: &Cx<S>, path: &str) -> Result<crate::browse::FileContent> {
        debug_assert!(
            !path.is_empty() && path.starts_with('/'),
            "read_file expects an absolute path"
        );
        let abs = to_absolute_path(path);

        // Bind prefix is a strict ancestor of the read path: dispatch
        // through the typed handler with the relative suffix.
        if let Some((route, parsed, suffix)) = self.match_bind_prefix(&abs) {
            let handler = (route.call)(cx, parsed).await?;
            return handler.read_file(cx, &suffix).await;
        }

        if let Some((route, parsed)) = self.match_file(&abs) {
            return match (route.call)(cx, parsed).await? {
                FileContent::Bytes(bytes) => Ok(crate::browse::FileContent::new(bytes)),
                FileContent::BytesWithAttrs { attrs, bytes } => {
                    Ok(crate::browse::FileContent::new(bytes).with_attrs(attrs))
                },
                FileContent::Stream(_) | FileContent::Range { .. } => {
                    Err(ProviderError::unimplemented(
                        "streamed and ranged file reads are reserved but not wired through the current host runtime",
                    ))
                },
            };
        }

        let (parent_rel, name) = split_parent_name(path)
            .ok_or_else(|| ProviderError::not_a_file(format!("path is not a file: {path}")))?;
        let parent_abs = to_absolute_path(parent_rel);
        let Some((route, parsed)) = self.match_dir(&parent_abs) else {
            return Err(ProviderError::not_found(format!("path not found: {path}")));
        };
        let projection = (route.call)(
            cx,
            parsed,
            DirIntent::ReadFile {
                name: name.to_string(),
            },
        )
        .await?;
        projected_file_from_projection(&projection, parent_rel, name)
    }

    pub async fn open_file(&self, cx: &Cx<S>, path: &str) -> Result<OpenedFile> {
        debug_assert!(
            !path.is_empty() && path.starts_with('/'),
            "open_file expects an absolute path"
        );
        let abs = to_absolute_path(path);

        if let Some((route, parsed, suffix)) = self.match_bind_prefix(&abs) {
            let handler = (route.call)(cx, parsed).await?;
            return handler.open_file(cx, &suffix).await;
        }

        if let Some((route, parsed)) = self.match_file(&abs) {
            return opened_file_from_content((route.call)(cx, parsed).await?);
        }

        Err(ProviderError::not_found(format!("path not found: {path}")))
    }

    fn match_dir(&self, absolute_path: &str) -> Option<(&DirHandlerRegistration<S>, Box<dyn Any>)> {
        best_route_match(&self.dirs, absolute_path)
    }

    fn match_file(
        &self,
        absolute_path: &str,
    ) -> Option<(&FileHandlerRegistration<S>, Box<dyn Any>)> {
        best_route_match(&self.files, absolute_path)
    }

    fn match_treeref(
        &self,
        absolute_path: &str,
    ) -> Option<(&TreeRefHandlerRegistration<S>, Box<dyn Any>)> {
        best_route_match(&self.treerefs, absolute_path)
    }

    /// Find a bind whose template is a strict ancestor of `path`.
    /// Returns the matched bind, parsed prefix captures, and the
    /// remaining absolute suffix (always begins with `/`, may be `/`
    /// only when the prefix exactly matches `path` — but this method
    /// requires a STRICT prefix and returns `None` in that case;
    /// `match_bind_at_or_below` is the variant that allows equality).
    fn match_bind_prefix(
        &self,
        path: &str,
    ) -> Option<(&BindRegistration<S>, Box<dyn Any>, String)> {
        match_bind_with(self, path, false)
    }

    /// Like `match_bind_prefix` but also matches when `path` exactly
    /// equals a bind template (suffix `/`).
    fn match_bind_at_or_below(
        &self,
        path: &str,
    ) -> Option<(&BindRegistration<S>, Box<dyn Any>, String)> {
        match_bind_with(self, path, true)
    }

    /// True iff `path` is an implicit directory node derivable from
    /// the route table without an explicit handler. The root is
    /// implicit whenever any routes are registered; a non-root path
    /// is implicit only when its last segment appears as a literal
    /// child of its parent in the static enumeration.
    fn is_implicit_prefix_dir(&self, absolute_path: &str) -> bool {
        if self.match_dir(absolute_path).is_some()
            || self.match_file(absolute_path).is_some()
            || self.match_treeref(absolute_path).is_some()
            || self
                .binds
                .iter()
                .any(|h| h.decl.pattern.matches_path(absolute_path))
        {
            return false;
        }
        if absolute_path == "/" {
            return !self.dirs.is_empty()
                || !self.files.is_empty()
                || !self.treerefs.is_empty()
                || !self.binds.is_empty();
        }
        let Some((parent, name)) = split_parent_name(absolute_path) else {
            return false;
        };
        let parent_abs = to_absolute_path(parent);
        self.static_entries_for_parent(&parent_abs)
            .iter()
            .any(|entry| entry.name() == name && entry.kind() == BrowseEntryKind::Directory)
    }

    /// True iff some registered route extends past `absolute_parent`
    /// and the segment immediately below is a capture or rest. A
    /// listing of such a parent cannot be authoritatively exhaustive.
    fn has_capture_child_under(&self, absolute_parent: &str) -> bool {
        let Some(parent_segments) = split_path(absolute_parent) else {
            return false;
        };
        let parent_depth = parent_segments.len();
        self.routes_extending_parent(&parent_segments)
            .any(|(pattern, _)| {
                matches!(
                    pattern.segments()[parent_depth],
                    PathSegment::Capture { .. } | PathSegment::Rest { .. }
                )
            })
    }

    /// Yields each registered pattern (with handler kind) that extends
    /// past `parent_segments` as a strict prefix. Callers can safely
    /// index `pattern.segments()[parent_segments.len()]` on every
    /// yielded pattern.
    fn routes_extending_parent<'a>(
        &'a self,
        parent_segments: &'a [&'a str],
    ) -> impl Iterator<Item = (&'a PathPattern, BrowseEntryKind)> + 'a {
        let dirs = self
            .dirs
            .iter()
            .map(|r| (&r.decl.pattern, BrowseEntryKind::Directory));
        let files = self
            .files
            .iter()
            .map(|r| (&r.decl.pattern, BrowseEntryKind::File));
        let treerefs = self
            .treerefs
            .iter()
            .map(|r| (&r.decl.pattern, BrowseEntryKind::Directory));
        let binds = self
            .binds
            .iter()
            .map(|r| (&r.decl.pattern, BrowseEntryKind::Directory));
        dirs.chain(files)
            .chain(treerefs)
            .chain(binds)
            .filter(move |(pattern, _)| pattern.accepts_as_strict_ancestor(parent_segments))
    }
}

/// Locate a bind registration whose template prefix-matches `path`.
/// Picks the longest (most-specific) match, parses prefix captures,
/// and computes the relative suffix to dispatch into the inner handler.
fn match_bind_with<'r, S>(
    registry: &'r MountRegistry<S>,
    path: &str,
    allow_equal: bool,
) -> Option<(&'r BindRegistration<S>, Box<dyn Any>, String)> {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let path_segment_count = if segments.len() == 1 && segments[0].is_empty() {
        0
    } else {
        segments.len()
    };

    // `validate()` has already sorted `binds` so longer (more
    // specific) templates come first; iterate as-is and the first
    // successful parse wins.
    for bind in &registry.binds {
        let bind_segments = bind.decl.pattern.segments().len();
        if bind_segments > path_segment_count {
            continue;
        }
        if !allow_equal && bind_segments == path_segment_count {
            continue;
        }
        let prefix = if bind_segments == 0 {
            "/".to_string()
        } else {
            format!("/{}", segments[..bind_segments].join("/"))
        };
        let Some(parsed) = (bind.parse)(&prefix) else {
            continue;
        };
        let suffix = if bind_segments == path_segment_count {
            "/".to_string()
        } else {
            format!("/{}", segments[bind_segments..].join("/"))
        };
        return Some((bind, parsed, suffix));
    }
    None
}

struct SubtreeDirHandlerRegistration<S, B> {
    decl: RouteDecl,
    parse: ParseFn,
    call: SubtreeDirCallFn<S, B>,
}

struct SubtreeFileHandlerRegistration<S, B> {
    decl: RouteDecl,
    parse: ParseFn,
    call: SubtreeFileCallFn<S, B>,
}

/// Per-type registry built once per `#[subtree] impl B { ... }` and
/// driven by `Handler<S>` trait dispatch. Simpler than `MountRegistry`:
/// no treerefs, no nested binds (a subtree cannot itself host
/// another bind in the current model).
pub struct SubtreeRegistry<S, B> {
    dirs: Vec<SubtreeDirHandlerRegistration<S, B>>,
    files: Vec<SubtreeFileHandlerRegistration<S, B>>,
}

impl<S, B> Default for SubtreeRegistry<S, B> {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            files: Vec::new(),
        }
    }
}

impl<S, B> SubtreeRegistry<S, B> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_dir(
        &mut self,
        template: &'static str,
        parse: ParseFn,
        call: SubtreeDirCallFn<S, B>,
    ) -> Result<()> {
        self.dirs.push(SubtreeDirHandlerRegistration {
            decl: RouteDecl {
                template,
                pattern: PathPattern::parse(template)
                    .map_err(|error| ProviderError::invalid_input(error.message().to_string()))?,
            },
            parse,
            call,
        });
        Ok(())
    }

    pub fn add_file(
        &mut self,
        template: &'static str,
        parse: ParseFn,
        call: SubtreeFileCallFn<S, B>,
    ) -> Result<()> {
        self.files.push(SubtreeFileHandlerRegistration {
            decl: RouteDecl {
                template,
                pattern: PathPattern::parse(template)
                    .map_err(|error| ProviderError::invalid_input(error.message().to_string()))?,
            },
            parse,
            call,
        });
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::<&'static str>::new();
        for decl in self
            .dirs
            .iter()
            .map(|entry| &entry.decl)
            .chain(self.files.iter().map(|entry| &entry.decl))
        {
            if !seen.insert(decl.template) {
                return Err(ProviderError::invalid_input(format!(
                    "duplicate subtree handler declared for {}",
                    decl.template
                )));
            }
        }
        validate_ambiguous_routes(&self.dirs, "subtree dir")?;
        validate_ambiguous_routes(&self.files, "subtree file")?;
        Ok(())
    }

    pub async fn lookup_child(
        &self,
        cx: &Cx<S>,
        bindings: &B,
        parent_path: &str,
        name: &str,
    ) -> Result<BrowseLookup> {
        let parent_abs = to_absolute_path(parent_path);
        let child_abs = join_absolute_path(&parent_abs, name);

        if let Some((route, parsed)) = self.match_dir(&child_abs) {
            let projection =
                (route.call)(cx, bindings, parsed, DirIntent::List { cursor: None }).await?;
            return projection_exact_lookup(&projection, &child_abs, BrowseEntry::dir(name), self);
        }

        if let Some((route, parsed)) = self.match_dir(&parent_abs) {
            let projection = (route.call)(
                cx,
                bindings,
                parsed,
                DirIntent::Lookup {
                    child: name.to_string(),
                },
            )
            .await?;
            return projection_lookup(
                &projection,
                &parent_abs,
                name,
                self.exact_entry_for_path(&child_abs),
                self,
            );
        }

        if let Some(target) = self.exact_entry_for_path(&child_abs) {
            let siblings = self.static_entries_for_parent(&parent_abs);
            let exhaustive = !self.has_capture_child_under(&parent_abs);
            return Ok(BrowseLookup::entry(target)
                .with_siblings(siblings)
                .exhaustive(exhaustive));
        }

        Ok(BrowseLookup::not_found())
    }

    pub async fn list_children(&self, cx: &Cx<S>, bindings: &B, path: &str) -> Result<BrowseList> {
        let abs = to_absolute_path(path);
        let static_entries = self.static_entries_for_parent(&abs);
        if let Some((route, parsed)) = self.match_dir(&abs) {
            let projection =
                (route.call)(cx, bindings, parsed, DirIntent::List { cursor: None }).await?;
            return projection_listing(&projection, static_entries).map(BrowseList::entries);
        }

        if self.is_implicit_prefix_dir(&abs) {
            let listing = if self.has_capture_child_under(&abs) {
                BrowseListing::partial(static_entries)
            } else {
                BrowseListing::complete(static_entries)
            };
            return Ok(BrowseList::entries(listing));
        }

        if self.match_file(&abs).is_some() {
            return Err(ProviderError::not_a_directory(format!("{path} is a file")));
        }

        Err(ProviderError::not_found(format!("path not found: {path}")))
    }

    pub async fn read_file(
        &self,
        cx: &Cx<S>,
        bindings: &B,
        path: &str,
    ) -> Result<crate::browse::FileContent> {
        let abs = to_absolute_path(path);
        if let Some((route, parsed)) = self.match_file(&abs) {
            return match (route.call)(cx, bindings, parsed).await? {
                FileContent::Bytes(bytes) => Ok(crate::browse::FileContent::new(bytes)),
                FileContent::BytesWithAttrs { attrs, bytes } => {
                    Ok(crate::browse::FileContent::new(bytes).with_attrs(attrs))
                },
                FileContent::Stream(_) | FileContent::Range { .. } => {
                    Err(ProviderError::unimplemented(
                        "streamed and ranged file reads are reserved but not wired through the current host runtime",
                    ))
                },
            };
        }

        let (parent_rel, name) = split_parent_name(path)
            .ok_or_else(|| ProviderError::not_a_file(format!("path is not a file: {path}")))?;
        let parent_abs = to_absolute_path(parent_rel);
        let Some((route, parsed)) = self.match_dir(&parent_abs) else {
            return Err(ProviderError::not_found(format!("path not found: {path}")));
        };
        let projection = (route.call)(
            cx,
            bindings,
            parsed,
            DirIntent::ReadFile {
                name: name.to_string(),
            },
        )
        .await?;
        projected_file_from_projection(&projection, parent_rel, name)
    }

    pub async fn open_file(&self, cx: &Cx<S>, bindings: &B, path: &str) -> Result<OpenedFile> {
        let abs = to_absolute_path(path);
        if let Some((route, parsed)) = self.match_file(&abs) {
            return opened_file_from_content((route.call)(cx, bindings, parsed).await?);
        }

        Err(ProviderError::not_found(format!("path not found: {path}")))
    }

    #[allow(clippy::type_complexity)]
    fn match_dir(
        &self,
        absolute_path: &str,
    ) -> Option<(&SubtreeDirHandlerRegistration<S, B>, Box<dyn Any>)> {
        best_route_match(&self.dirs, absolute_path)
    }

    #[allow(clippy::type_complexity)]
    fn match_file(
        &self,
        absolute_path: &str,
    ) -> Option<(&SubtreeFileHandlerRegistration<S, B>, Box<dyn Any>)> {
        best_route_match(&self.files, absolute_path)
    }

    fn is_implicit_prefix_dir(&self, absolute_path: &str) -> bool {
        if self.match_dir(absolute_path).is_some() || self.match_file(absolute_path).is_some() {
            return false;
        }
        if absolute_path == "/" {
            return !self.dirs.is_empty() || !self.files.is_empty();
        }
        let Some((parent, name)) = split_parent_name(absolute_path) else {
            return false;
        };
        let parent_abs = to_absolute_path(parent);
        self.static_entries_for_parent(&parent_abs)
            .iter()
            .any(|entry| entry.name() == name && entry.kind() == BrowseEntryKind::Directory)
    }

    fn has_capture_child_under(&self, absolute_parent: &str) -> bool {
        let Some(parent_segments) = split_path(absolute_parent) else {
            return false;
        };
        let parent_depth = parent_segments.len();
        self.routes_extending_parent(&parent_segments)
            .any(|(pattern, _)| {
                matches!(
                    pattern.segments()[parent_depth],
                    PathSegment::Capture { .. } | PathSegment::Rest { .. }
                )
            })
    }

    fn routes_extending_parent<'a>(
        &'a self,
        parent_segments: &'a [&'a str],
    ) -> impl Iterator<Item = (&'a PathPattern, BrowseEntryKind)> + 'a {
        let dirs = self
            .dirs
            .iter()
            .map(|r| (&r.decl.pattern, BrowseEntryKind::Directory));
        let files = self
            .files
            .iter()
            .map(|r| (&r.decl.pattern, BrowseEntryKind::File));
        dirs.chain(files)
            .filter(move |(pattern, _)| pattern.accepts_as_strict_ancestor(parent_segments))
    }
}

impl<S, B> StaticChildren for SubtreeRegistry<S, B> {
    fn static_entries_for_parent(&self, absolute_parent: &str) -> Vec<BrowseEntry> {
        let Some(parent_segments) = split_path(absolute_parent) else {
            return Vec::new();
        };
        let parent_depth = parent_segments.len();

        let mut entries = std::collections::BTreeMap::<String, BrowseEntry>::new();
        for (pattern, kind) in self.routes_extending_parent(&parent_segments) {
            let route_segments = pattern.segments();
            let PathSegment::Literal(name) = &route_segments[parent_depth] else {
                continue;
            };
            let extends_below = route_segments.len() > parent_depth + 1;
            entries.entry(name.clone()).or_insert_with(|| {
                if extends_below || matches!(kind, BrowseEntryKind::Directory) {
                    BrowseEntry::dir(name.as_str())
                } else {
                    BrowseEntry::file(name.as_str(), default_static_file_proj())
                }
            });
        }
        entries.into_values().collect()
    }

    fn reserved_static_names(&self, absolute_parent: &str) -> std::collections::BTreeSet<String> {
        self.static_entries_for_parent(absolute_parent)
            .into_iter()
            .map(|entry| entry.name().to_string())
            .collect()
    }

    fn exact_entry_for_path(&self, absolute_path: &str) -> Option<BrowseEntry> {
        if self.match_dir(absolute_path).is_some() {
            let name = child_name(absolute_path)?;
            return Some(BrowseEntry::dir(name));
        }
        if self.match_file(absolute_path).is_some() {
            let name = child_name(absolute_path)?;
            return Some(BrowseEntry::file(name, default_static_file_proj()));
        }
        if self.is_implicit_prefix_dir(absolute_path) {
            let name = child_name(absolute_path)?;
            return Some(BrowseEntry::dir(name));
        }
        None
    }
}

impl<S> StaticChildren for MountRegistry<S> {
    fn static_entries_for_parent(&self, absolute_parent: &str) -> Vec<BrowseEntry> {
        let Some(parent_segments) = split_path(absolute_parent) else {
            return Vec::new();
        };
        let parent_depth = parent_segments.len();

        let mut entries = std::collections::BTreeMap::<String, BrowseEntry>::new();
        for (pattern, kind) in self.routes_extending_parent(&parent_segments) {
            let route_segments = pattern.segments();
            let PathSegment::Literal(name) = &route_segments[parent_depth] else {
                continue;
            };
            // A route extending past the child position forces a dir,
            // regardless of its terminal kind: deeper segments mean the
            // child is the parent of further paths.
            let extends_below = route_segments.len() > parent_depth + 1;
            entries.entry(name.clone()).or_insert_with(|| {
                if extends_below || matches!(kind, BrowseEntryKind::Directory) {
                    BrowseEntry::dir(name.as_str())
                } else {
                    BrowseEntry::file(name.as_str(), default_static_file_proj())
                }
            });
        }
        entries.into_values().collect()
    }

    fn reserved_static_names(&self, absolute_parent: &str) -> std::collections::BTreeSet<String> {
        self.static_entries_for_parent(absolute_parent)
            .into_iter()
            .map(|entry| entry.name().to_string())
            .collect()
    }

    fn exact_entry_for_path(&self, absolute_path: &str) -> Option<BrowseEntry> {
        if self.match_dir(absolute_path).is_some()
            || self.match_treeref(absolute_path).is_some()
            || self
                .binds
                .iter()
                .any(|h| h.decl.pattern.matches_path(absolute_path))
        {
            let name = child_name(absolute_path)?;
            return Some(BrowseEntry::dir(name));
        }
        if self.match_file(absolute_path).is_some() {
            let name = child_name(absolute_path)?;
            return Some(BrowseEntry::file(name, default_static_file_proj()));
        }
        if self.is_implicit_prefix_dir(absolute_path) {
            let name = child_name(absolute_path)?;
            return Some(BrowseEntry::dir(name));
        }
        None
    }
}

fn merge_projection_entries(
    projection: &Projection,
    static_entries: Vec<BrowseEntry>,
) -> std::collections::BTreeMap<String, BrowseEntry> {
    let mut entries = static_entries
        .into_iter()
        .map(|entry| (entry.name().to_string(), entry))
        .collect::<std::collections::BTreeMap<_, _>>();

    for entry in &projection.entries {
        let browse_entry = match entry {
            ProjectionEntry::Directory { name } => BrowseEntry::dir(name),
            ProjectionEntry::File { name, file } => BrowseEntry::file(name, file.clone()),
        };

        entries.insert(entry.name().to_string(), browse_entry);
    }

    entries
}

fn projected_file_from_projection(
    projection: &Projection,
    parent_path: &str,
    name: &str,
) -> Result<crate::browse::FileContent> {
    if !projection.errors.is_empty() {
        return Err(ProviderError::invalid_input(projection.errors.join("; ")));
    }
    let entry = projection
        .entries
        .iter()
        .find(|entry| entry.name() == name)
        .ok_or_else(|| ProviderError::not_found(format!("path not found: {name}")))?;
    let ProjectionEntry::File { file, .. } = entry else {
        return Err(ProviderError::not_a_file(format!(
            "path is not a file: {name}"
        )));
    };
    let Some(bytes) = file.inline_bytes() else {
        return Err(ProviderError::not_found(format!(
            "projection entry {name} has no eager bytes"
        )));
    };
    let mut effects = projection.effects.clone();
    for entry in projection
        .entries
        .iter()
        .filter(|entry| entry.name() != name)
    {
        match entry {
            ProjectionEntry::Directory { name } => {
                effects.project_dir(join_provider_path(parent_path, name))?;
            },
            ProjectionEntry::File { name, file } => {
                effects.project_file(join_provider_path(parent_path, name), file.clone())?;
            },
        }
    }
    Ok(crate::browse::FileContent::new(bytes.to_vec())
        .with_attrs(crate::browse::deferred_full_attrs_for_read(file))
        .with_effects(effects))
}

fn opened_file_from_content(content: FileContent) -> Result<OpenedFile> {
    match content {
        FileContent::Bytes(_) | FileContent::BytesWithAttrs { .. } => {
            Err(ProviderError::invalid_input(
                "open-file requires FileContent::ranged or FileContent::range_bytes",
            ))
        },
        FileContent::Range { file, reader } => {
            file.validate()
                .map_err(|error| ProviderError::invalid_input(error.message().to_string()))?;
            Ok(OpenedFile::new(file.attrs, reader))
        },
        FileContent::Stream(_) => Err(ProviderError::unimplemented(
            "streamed file reads are not wired through the current host runtime",
        )),
    }
}

fn projection_listing(
    projection: &Projection,
    static_entries: Vec<BrowseEntry>,
) -> Result<BrowseListing> {
    if !projection.errors.is_empty() {
        return Err(ProviderError::invalid_input(projection.errors.join("; ")));
    }
    let entries = merge_projection_entries(projection, static_entries);

    let exhaustive = match projection.page.as_ref() {
        Some(PageStatus::More(_)) => false,
        Some(PageStatus::Exhaustive) | None => true,
    };

    let listing = if exhaustive {
        BrowseListing::complete(entries.into_values())
    } else {
        BrowseListing::partial(entries.into_values())
    };
    Ok(listing.with_effects(projection.effects.clone()))
}

/// Source of statically-derived sibling/exact-entry information for a
/// projection lookup. Both `MountRegistry<S>` and `SubtreeRegistry<S, B>`
/// implement this so the projection helpers can be reused.
pub(crate) trait StaticChildren {
    fn static_entries_for_parent(&self, absolute_parent: &str) -> Vec<BrowseEntry>;
    fn reserved_static_names(&self, absolute_parent: &str) -> std::collections::BTreeSet<String>;
    fn exact_entry_for_path(&self, absolute_path: &str) -> Option<BrowseEntry>;
}

fn projection_lookup<R: StaticChildren>(
    projection: &Projection,
    absolute_parent: &str,
    target_name: &str,
    fallback_target: Option<BrowseEntry>,
    registry: &R,
) -> Result<BrowseLookup> {
    if !projection.errors.is_empty() {
        return Err(ProviderError::invalid_input(projection.errors.join("; ")));
    }

    let reserved = registry.reserved_static_names(absolute_parent);
    let siblings = merge_projection_entries(
        projection,
        registry.static_entries_for_parent(absolute_parent),
    );
    let target = if let Some(entry) = siblings.get(target_name).cloned() {
        Some(entry)
    } else if reserved.contains(target_name) {
        Some(
            registry
                .exact_entry_for_path(&join_absolute_path(absolute_parent, target_name))
                .ok_or_else(|| ProviderError::internal("missing reserved entry"))?,
        )
    } else {
        siblings.get(target_name).cloned().or(fallback_target)
    };

    let exhaustive = matches!(projection.page.as_ref(), Some(PageStatus::Exhaustive));

    let lookup = target.map_or_else(BrowseLookup::not_found, BrowseLookup::entry);
    Ok(lookup
        .with_siblings(siblings.into_values())
        .with_effects(projection.effects.clone())
        .exhaustive(exhaustive))
}

fn projection_exact_lookup<R: StaticChildren>(
    projection: &Projection,
    absolute_path: &str,
    target: BrowseEntry,
    registry: &R,
) -> Result<BrowseLookup> {
    if !projection.errors.is_empty() {
        return Err(ProviderError::invalid_input(projection.errors.join("; ")));
    }

    let (parent_abs, target_name) = split_parent_name(absolute_path)
        .ok_or_else(|| ProviderError::internal("exact lookup path has no parent"))?;
    let mut siblings = registry.static_entries_for_parent(&to_absolute_path(parent_abs));
    siblings.retain(|entry| entry.name() != target_name);

    let mut effects = projection.effects.clone();
    effects.project_dir(absolute_path)?;
    let projected_children = merge_projection_entries(
        projection,
        registry.static_entries_for_parent(absolute_path),
    );
    for entry in projected_children.into_values() {
        let path = join_provider_path(absolute_path, entry.name());
        match entry.kind() {
            BrowseEntryKind::Directory => {
                effects.project_dir(path)?;
            },
            BrowseEntryKind::File => {
                let file = entry
                    .file_proj()
                    .cloned()
                    .ok_or_else(|| ProviderError::internal("file projection missing data"))?;
                effects.project_file(path, file)?;
            },
        }
    }

    Ok(BrowseLookup::entry(target)
        .with_siblings(siblings)
        .with_effects(effects)
        .exhaustive(false))
}

/// Whether `inner`'s segment sequence starts with all of `outer`'s
/// segments (literal-equals-literal, capture-equals-capture-by-prefix).
/// Used by the exclusivity validator to catch routes declared under a
/// bind prefix.
fn pattern_starts_with(inner: &PathPattern, outer: &PathPattern) -> bool {
    let outer_segments = outer.segments();
    let inner_segments = inner.segments();
    if inner_segments.len() < outer_segments.len() {
        return false;
    }
    for (i, o) in inner_segments.iter().zip(outer_segments.iter()) {
        let same = match (i, o) {
            (
                omnifs_mount_schema::PathSegment::Literal(a),
                omnifs_mount_schema::PathSegment::Literal(b),
            ) => a == b,
            (
                omnifs_mount_schema::PathSegment::Capture { .. },
                omnifs_mount_schema::PathSegment::Capture { .. },
            )
            | (
                omnifs_mount_schema::PathSegment::Rest { .. },
                omnifs_mount_schema::PathSegment::Rest { .. },
            ) => true,
            _ => false,
        };
        if !same {
            return false;
        }
    }
    true
}

trait RegisteredRoute {
    fn decl(&self) -> &RouteDecl;
    fn parse(&self, path: &str) -> Option<Box<dyn Any>>;
}

impl<S> RegisteredRoute for DirHandlerRegistration<S> {
    fn decl(&self) -> &RouteDecl {
        &self.decl
    }

    fn parse(&self, path: &str) -> Option<Box<dyn Any>> {
        (self.parse)(path)
    }
}

impl<S> RegisteredRoute for FileHandlerRegistration<S> {
    fn decl(&self) -> &RouteDecl {
        &self.decl
    }

    fn parse(&self, path: &str) -> Option<Box<dyn Any>> {
        (self.parse)(path)
    }
}

impl<S> RegisteredRoute for TreeRefHandlerRegistration<S> {
    fn decl(&self) -> &RouteDecl {
        &self.decl
    }

    fn parse(&self, path: &str) -> Option<Box<dyn Any>> {
        (self.parse)(path)
    }
}

impl<S> RegisteredRoute for BindRegistration<S> {
    fn decl(&self) -> &RouteDecl {
        &self.decl
    }

    fn parse(&self, path: &str) -> Option<Box<dyn Any>> {
        (self.parse)(path)
    }
}

impl<S, B> RegisteredRoute for SubtreeDirHandlerRegistration<S, B> {
    fn decl(&self) -> &RouteDecl {
        &self.decl
    }

    fn parse(&self, path: &str) -> Option<Box<dyn Any>> {
        (self.parse)(path)
    }
}

impl<S, B> RegisteredRoute for SubtreeFileHandlerRegistration<S, B> {
    fn decl(&self) -> &RouteDecl {
        &self.decl
    }

    fn parse(&self, path: &str) -> Option<Box<dyn Any>> {
        (self.parse)(path)
    }
}

/// Walk shape-matching candidates in precedence order and return the
/// first whose parse function accepts `absolute_path`. Per-segment
/// validators participate in match candidacy: a parse rejection means
/// "this candidate does not own this path", and the dispatcher falls
/// through to the next-most-specific candidate.
fn best_route_match<'a, R>(routes: &'a [R], absolute_path: &str) -> Option<(&'a R, Box<dyn Any>)>
where
    R: RegisteredRoute,
{
    let mut candidates: Vec<&R> = routes
        .iter()
        .filter(|route| route.decl().pattern.matches_path(absolute_path))
        .collect();
    candidates.sort_by(|a, b| {
        b.decl()
            .pattern
            .precedence_key()
            .cmp(&a.decl().pattern.precedence_key())
    });
    candidates
        .into_iter()
        .find_map(|route| route.parse(absolute_path).map(|parsed| (route, parsed)))
}

fn validate_ambiguous_routes<R>(routes: &[R], kind: &str) -> Result<()>
where
    R: RegisteredRoute,
{
    for (index, left) in routes.iter().enumerate() {
        for right in routes.iter().skip(index + 1) {
            if left.decl().pattern.is_ambiguous_with(&right.decl().pattern) {
                return Err(ProviderError::invalid_input(format!(
                    "ambiguous {kind} handlers {} and {}",
                    left.decl().template,
                    right.decl().template
                )));
            }
        }
    }
    Ok(())
}

fn default_static_file_proj() -> FileProj {
    FileProj::deferred(Size::Unknown, ReadMode::Full, Stability::Immutable)
}

fn is_valid_rel_segment(segment: &str) -> bool {
    !segment.is_empty() && !segment.contains('/') && segment != "." && segment != ".."
}

fn to_absolute_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn join_absolute_path(parent: &str, child: &str) -> String {
    if parent == "/" {
        format!("/{child}")
    } else {
        format!("{parent}/{child}")
    }
}

fn join_provider_path(parent: &str, child: &str) -> String {
    let parent = parent.trim_matches('/');
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}/{child}")
    }
}

fn child_name(path: &str) -> Option<&str> {
    if path == "/" {
        None
    } else {
        path.rsplit('/').next()
    }
}

fn split_parent_name(path: &str) -> Option<(&str, &str)> {
    if path.is_empty() {
        return None;
    }
    match path.rsplit_once('/') {
        Some((parent, name)) if !name.is_empty() => Some((parent, name)),
        None => Some(("", path)),
        _ => None,
    }
}
