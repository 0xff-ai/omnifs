use crate::browse::{
    Entry as BrowseEntry, EntryKind as BrowseEntryKind, List as BrowseList,
    Listing as BrowseListing, Lookup as BrowseLookup, Preload as BrowsePreload, ProjectedFile,
};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use omnifs_mount_schema::PathPattern;
use serde::Serialize;
use std::any::Any;
use std::future::Future;
use std::num::NonZeroU64;
use std::pin::Pin;

const DEFAULT_FILE_SIZE_BYTES: u64 = 4096;
pub const MAX_PROJECTED_BYTES: usize = 64 * 1024;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cursor {
    Opaque(String),
    Page(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirIntent<'a> {
    Lookup { child: &'a str },
    List { cursor: Option<Cursor> },
    ReadProjectedFile { name: &'a str },
}

/// Directory handler context: a `Cx<S>` paired with the request intent.
///
/// Dir handlers serve three operations (lookup, list, read-projected-file);
/// `DirCx` carries which one the host asked for. Derefs to `Cx<S>` so all
/// the usual context methods (`.http()`, `.git()`, `.state()`) work directly.
pub struct DirCx<'a, S> {
    cx: &'a Cx<S>,
    intent: DirIntent<'a>,
}

impl<'a, S> DirCx<'a, S> {
    pub fn new(cx: &'a Cx<S>, intent: DirIntent<'a>) -> Self {
        Self { cx, intent }
    }

    pub fn intent(&self) -> &DirIntent<'a> {
        &self.intent
    }
}

impl<S> std::ops::Deref for DirCx<'_, S> {
    type Target = Cx<S>;

    fn deref(&self) -> &Cx<S> {
        self.cx
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageStatus {
    Exhaustive,
    More(Cursor),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub size: NonZeroU64,
}

impl FileStat {
    pub fn placeholder() -> Self {
        Self {
            size: placeholder_size(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionKind {
    Directory,
    File,
}

#[derive(Clone, Debug)]
struct ProjectionEntry {
    name: String,
    kind: ProjectionKind,
    stat: Option<FileStat>,
    bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
pub struct Projection {
    entries: Vec<ProjectionEntry>,
    page: Option<PageStatus>,
    preload: Vec<BrowsePreload>,
    error: Option<String>,
}

impl Projection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn dir(&mut self, name: impl Into<String>) {
        let _ = self.push_entry(name.into(), ProjectionKind::Directory, None, None);
    }

    pub fn file(&mut self, name: impl Into<String>) {
        let _ = self.push_entry(
            name.into(),
            ProjectionKind::File,
            Some(FileStat::placeholder()),
            None,
        );
    }

    pub fn file_with_stat(&mut self, name: impl Into<String>, stat: FileStat) {
        let _ = self.push_entry(name.into(), ProjectionKind::File, Some(stat), None);
    }

    pub fn file_with_content(&mut self, name: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        if bytes.len() > MAX_PROJECTED_BYTES {
            let _ = self.reject::<()>(format!(
                "projected file exceeds eager byte limit of {MAX_PROJECTED_BYTES} bytes"
            ));
            return;
        }
        let stat = NonZeroU64::new(u64::try_from(bytes.len()).unwrap_or(DEFAULT_FILE_SIZE_BYTES))
            .map_or_else(FileStat::placeholder, |size| FileStat { size });
        let _ = self.push_entry(name.into(), ProjectionKind::File, Some(stat), Some(bytes));
    }

    pub fn page(&mut self, status: PageStatus) {
        self.page = Some(status);
    }

    /// Hand file content to the host so a later read of `path` can be
    /// served without another provider round trip. Accumulates into the
    /// `preload` field of the eventual `dir-listing`. Empty paths are
    /// dropped silently. Empty content is preserved as a valid file.
    pub fn preload(&mut self, path: impl Into<String>, content: impl Into<Vec<u8>>) {
        let path = path.into();
        let content = content.into();
        if path.is_empty() {
            return;
        }
        self.preload.push(BrowsePreload::file(path, content));
    }

    /// Hand a batch of file contents to the host so later reads of each
    /// path are served without provider round trips.
    pub fn preload_many<I, P, B>(&mut self, files: I)
    where
        I: IntoIterator<Item = (P, B)>,
        P: Into<String>,
        B: Into<Vec<u8>>,
    {
        for (path, content) in files {
            self.preload(path, content);
        }
    }

    /// Hand entry metadata to the host so a later lookup of `path` can
    /// be served without another provider round trip.
    ///
    /// If the same preload batch also contains direct children under
    /// this directory entry, the host may materialize a partial cached
    /// listing from those children. A directory preload by itself does
    /// not cache an empty listing.
    pub fn preload_entry(
        &mut self,
        path: impl Into<String>,
        kind: BrowseEntryKind,
        size: Option<NonZeroU64>,
    ) {
        let path = path.into();
        if path.is_empty() {
            return;
        }
        self.preload.push(BrowsePreload::entry(path, kind, size));
    }

    /// Hand directory metadata to the host so a later lookup of `path`
    /// can be served without another provider round trip.
    pub fn preload_dir(&mut self, path: impl Into<String>) {
        self.preload_entry(path, BrowseEntryKind::Directory, None);
    }

    pub fn into_error(self) -> Option<String> {
        self.error
    }

    fn push_entry(
        &mut self,
        name: String,
        kind: ProjectionKind,
        stat: Option<FileStat>,
        bytes: Option<Vec<u8>>,
    ) -> Result<()> {
        if !is_valid_rel_segment(&name) {
            return self.reject(format!("invalid child name {name:?}"));
        }
        if self.entries.iter().any(|entry| entry.name == name) {
            return self.reject(format!("duplicate child name {name:?}"));
        }
        self.entries.push(ProjectionEntry {
            name,
            kind,
            stat,
            bytes,
        });
        Ok(())
    }

    fn record_error(&mut self, message: String) {
        if self.error.is_none() {
            self.error = Some(message);
        }
    }

    fn reject<T>(&mut self, message: String) -> Result<T> {
        self.record_error(message.clone());
        Err(ProviderError::invalid_input(message))
    }
}

#[derive(Clone, Debug)]
pub enum FileContent {
    Bytes(Vec<u8>),
    Stream(StreamHandle),
    Range { len: u64, reader: RangeReaderHandle },
}

impl FileContent {
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(bytes.into())
    }

    pub fn stream(handle: StreamHandle) -> Self {
        Self::Stream(handle)
    }

    pub fn range(len: u64, reader: RangeReaderHandle) -> Self {
        Self::Range { len, reader }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamHandle {
    pub id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeReaderHandle {
    pub id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubtreeRef {
    pub tree_ref: u64,
}

impl SubtreeRef {
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
type DirCallFn<S> = for<'a> fn(&'a Cx<S>, Box<dyn Any>, DirIntent<'a>) -> BoxFuture<'a, Projection>;
type FileCallFn<S> = for<'a> fn(&'a Cx<S>, Box<dyn Any>) -> BoxFuture<'a, FileContent>;
type SubtreeCallFn<S> = for<'a> fn(&'a Cx<S>, Box<dyn Any>) -> BoxFuture<'a, SubtreeRef>;

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

struct SubtreeHandlerRegistration<S> {
    decl: RouteDecl,
    parse: ParseFn,
    call: SubtreeCallFn<S>,
}

pub struct MountRegistry<S> {
    dirs: Vec<DirHandlerRegistration<S>>,
    files: Vec<FileHandlerRegistration<S>>,
    subtrees: Vec<SubtreeHandlerRegistration<S>>,
}

impl<S> Default for MountRegistry<S> {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            files: Vec::new(),
            subtrees: Vec::new(),
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

    pub fn add_subtree(
        &mut self,
        template: &'static str,
        parse: ParseFn,
        call: SubtreeCallFn<S>,
    ) -> Result<()> {
        self.subtrees.push(SubtreeHandlerRegistration {
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
        // Within-kind dedup: two #[dir("X")] (or two #[file("X")], or two
        // #[subtree("X")]) handlers on the same template are always wrong.
        // Cross-kind co-existence of dir+file is allowed — the dispatch
        // routes by request kind. Subtrees still take over the path
        // entirely, so they're mutually exclusive with both dirs and files
        // at the same template.
        fn dedup_within<'a, I>(decls: I, kind: &str) -> Result<()>
        where
            I: IntoIterator<Item = &'a RouteDecl>,
        {
            let mut seen = std::collections::BTreeSet::<&'static str>::new();
            for decl in decls {
                if !seen.insert(decl.template) {
                    return Err(ProviderError::invalid_input(format!(
                        "duplicate {kind} handler declared for {}",
                        decl.template
                    )));
                }
            }
            Ok(())
        }
        dedup_within(self.dirs.iter().map(|e| &e.decl), "dir")?;
        dedup_within(self.files.iter().map(|e| &e.decl), "file")?;
        dedup_within(self.subtrees.iter().map(|e| &e.decl), "subtree")?;

        let subtree_templates: std::collections::BTreeSet<&'static str> =
            self.subtrees.iter().map(|e| e.decl.template).collect();
        for decl in self
            .dirs
            .iter()
            .map(|e| &e.decl)
            .chain(self.files.iter().map(|e| &e.decl))
        {
            if subtree_templates.contains(decl.template) {
                return Err(ProviderError::invalid_input(format!(
                    "handler {} conflicts with a subtree handler on the same template",
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
            .chain(self.subtrees.iter().map(|entry| &entry.decl))
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
        validate_ambiguous_routes(&self.subtrees, "subtree")?;
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

        if let Some((route, parsed)) = self.match_subtree(&child_abs) {
            let tree_ref = (route.call)(cx, parsed).await?.tree_ref;
            return Ok(BrowseLookup::subtree(tree_ref));
        }

        // When a #[dir] and #[file] both match the child path (the dir+file
        // co-existence case enabled for content-determined routing under
        // rest-captured templates), the structural match can't tell us the
        // child's kind. Defer to the parent dir handler's projection
        // verdict by skipping the direct-match shortcuts below.
        let dir_match = self.match_dir(&child_abs);
        let ambiguous = dir_match.is_some() && self.match_file(&child_abs).is_some();

        if !ambiguous {
            if let Some((route, parsed)) = dir_match {
                // Exact dir lookups can warm the looked-up directory's adjacent cache shape.
                let projection = (route.call)(cx, parsed, DirIntent::List { cursor: None }).await?;
                return projection_exact_lookup(
                    &projection,
                    &child_abs,
                    BrowseEntry::dir(name),
                    self,
                );
            }

            if let Some(target) = self.exact_entry_for_path(&child_abs) {
                let siblings = self.static_entries_for_parent(&parent_abs);
                return Ok(BrowseLookup::entry(target)
                    .with_siblings(siblings)
                    .exhaustive(true));
            }
        }

        let Some((route, parsed)) = self.match_dir(&parent_abs) else {
            return Ok(BrowseLookup::not_found());
        };
        let projection = (route.call)(cx, parsed, DirIntent::Lookup { child: name }).await?;
        projection_lookup(&projection, &parent_abs, name, None, self)
    }

    pub async fn list_children(&self, cx: &Cx<S>, path: &str) -> Result<BrowseList> {
        debug_assert!(
            path.is_empty() || path.starts_with('/'),
            "list_children expects an absolute path"
        );
        let abs = to_absolute_path(path);

        if let Some((route, parsed)) = self.match_subtree(&abs) {
            let tree_ref = (route.call)(cx, parsed).await?.tree_ref;
            return Ok(BrowseList::subtree(tree_ref));
        }

        let static_entries = self.static_entries_for_parent(&abs);
        if let Some((route, parsed)) = self.match_dir(&abs) {
            let projection = (route.call)(cx, parsed, DirIntent::List { cursor: None }).await?;
            return projection_listing(&projection, static_entries).map(BrowseList::entries);
        }

        if !static_entries.is_empty() {
            return Ok(BrowseList::entries(BrowseListing::complete(static_entries)));
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
        if let Some((route, parsed)) = self.match_file(&abs) {
            return match (route.call)(cx, parsed).await? {
                FileContent::Bytes(bytes) => Ok(crate::browse::FileContent::new(bytes)),
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
        let projection = (route.call)(cx, parsed, DirIntent::ReadProjectedFile { name }).await?;
        projected_file_from_projection(&projection, name)
    }

    fn exact_entry_for_path(&self, absolute_path: &str) -> Option<BrowseEntry> {
        if self.match_dir(absolute_path).is_some() || self.match_subtree(absolute_path).is_some() {
            let name = child_name(absolute_path)?;
            return Some(BrowseEntry::dir(name));
        }
        if self.match_file(absolute_path).is_some() {
            let name = child_name(absolute_path)?;
            return Some(BrowseEntry::file(name, placeholder_size()));
        }
        None
    }

    fn static_entries_for_parent(&self, absolute_parent: &str) -> Vec<BrowseEntry> {
        let mut entries = std::collections::BTreeMap::<String, BrowseEntry>::new();
        for route in &self.dirs {
            add_static_entry(&mut entries, &route.decl, absolute_parent, true);
        }
        for route in &self.files {
            add_static_entry(&mut entries, &route.decl, absolute_parent, false);
        }
        for route in &self.subtrees {
            add_static_entry(&mut entries, &route.decl, absolute_parent, true);
        }
        entries.into_values().collect()
    }

    fn reserved_static_names(&self, absolute_parent: &str) -> std::collections::BTreeSet<String> {
        self.static_entries_for_parent(absolute_parent)
            .into_iter()
            .map(|entry| entry.name().to_string())
            .collect()
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

    fn match_subtree(
        &self,
        absolute_path: &str,
    ) -> Option<(&SubtreeHandlerRegistration<S>, Box<dyn Any>)> {
        best_route_match(&self.subtrees, absolute_path)
    }
}

fn merge_projection_entries(
    projection: &Projection,
    static_entries: Vec<BrowseEntry>,
) -> Result<(
    std::collections::BTreeMap<String, BrowseEntry>,
    Vec<ProjectedFile>,
)> {
    let mut entries = static_entries
        .into_iter()
        .map(|entry| (entry.name().to_string(), entry))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut sibling_files = Vec::new();

    for entry in &projection.entries {
        let browse_entry = match entry.kind {
            ProjectionKind::Directory => BrowseEntry::dir(&entry.name),
            ProjectionKind::File => {
                let size = entry.stat.map_or_else(placeholder_size, |stat| stat.size);
                if let Some(bytes) = &entry.bytes {
                    sibling_files.push(ProjectedFile::new(&entry.name, bytes.clone()));
                }
                BrowseEntry::file(&entry.name, size)
            },
        };

        if entries.insert(entry.name.clone(), browse_entry).is_some() {
            return Err(ProviderError::invalid_input(format!(
                "child {:?} was emitted more than once",
                entry.name
            )));
        }
    }

    Ok((entries, sibling_files))
}

fn projected_file_from_projection(
    projection: &Projection,
    name: &str,
) -> Result<crate::browse::FileContent> {
    if let Some(error) = projection.error.as_deref() {
        return Err(ProviderError::invalid_input(error.to_string()));
    }
    let entry = projection
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| ProviderError::not_found(format!("path not found: {name}")))?;
    let Some(bytes) = &entry.bytes else {
        return Err(ProviderError::not_found(format!(
            "projected file {name} has no eager bytes"
        )));
    };
    let sibling_files = projection
        .entries
        .iter()
        .filter(|entry| entry.name != name)
        .filter_map(|entry| {
            entry
                .bytes
                .as_ref()
                .map(|bytes| ProjectedFile::new(&entry.name, bytes.clone()))
        })
        .collect::<Vec<_>>();
    Ok(crate::browse::FileContent::new(bytes.clone()).with_sibling_files(sibling_files))
}

fn projection_listing(
    projection: &Projection,
    static_entries: Vec<BrowseEntry>,
) -> Result<BrowseListing> {
    if let Some(error) = projection.error.as_deref() {
        return Err(ProviderError::invalid_input(error.to_string()));
    }
    let (mut entries, sibling_files) = merge_projection_entries(projection, static_entries)?;
    for projected in sibling_files {
        if let Some(entry) = entries.get_mut(projected.name()) {
            *entry = entry.clone().projected(projected.content().to_vec());
        }
    }

    let exhaustive = match projection.page.as_ref() {
        Some(PageStatus::More(_)) => false,
        Some(PageStatus::Exhaustive) | None => true,
    };

    let listing = if exhaustive {
        BrowseListing::complete(entries.into_values())
    } else {
        BrowseListing::partial(entries.into_values())
    };
    Ok(listing.with_preload(projection.preload.iter().cloned()))
}

fn projection_lookup<S>(
    projection: &Projection,
    absolute_parent: &str,
    target_name: &str,
    fallback_target: Option<BrowseEntry>,
    registry: &MountRegistry<S>,
) -> Result<BrowseLookup> {
    if let Some(error) = projection.error.as_deref() {
        return Err(ProviderError::invalid_input(error.to_string()));
    }

    let reserved = registry.reserved_static_names(absolute_parent);
    let (siblings, sibling_files) = merge_projection_entries(
        projection,
        registry.static_entries_for_parent(absolute_parent),
    )?;
    let target = if reserved.contains(target_name) {
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
        .with_sibling_files(sibling_files)
        .with_preload(projection.preload.iter().cloned())
        .exhaustive(exhaustive))
}

fn projection_exact_lookup<S>(
    projection: &Projection,
    absolute_path: &str,
    target: BrowseEntry,
    registry: &MountRegistry<S>,
) -> Result<BrowseLookup> {
    if let Some(error) = projection.error.as_deref() {
        return Err(ProviderError::invalid_input(error.to_string()));
    }

    let (siblings, sibling_files) = merge_projection_entries(
        projection,
        registry.static_entries_for_parent(absolute_path),
    )?;
    let exhaustive = !matches!(projection.page.as_ref(), Some(PageStatus::More(_)));

    Ok(BrowseLookup::entry(target)
        .with_siblings(siblings.into_values())
        .with_sibling_files(sibling_files)
        .with_preload(projection.preload.iter().cloned())
        .exhaustive(exhaustive))
}

fn add_static_entry(
    entries: &mut std::collections::BTreeMap<String, BrowseEntry>,
    decl: &RouteDecl,
    absolute_parent: &str,
    is_dir: bool,
) {
    if !decl.pattern.matches_parent_path(absolute_parent) {
        return;
    }
    let Some(name) = decl.pattern.static_child() else {
        return;
    };
    entries.entry(name.to_string()).or_insert_with(|| {
        if is_dir {
            BrowseEntry::dir(name)
        } else {
            BrowseEntry::file(name, placeholder_size())
        }
    });
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

impl<S> RegisteredRoute for SubtreeHandlerRegistration<S> {
    fn decl(&self) -> &RouteDecl {
        &self.decl
    }

    fn parse(&self, path: &str) -> Option<Box<dyn Any>> {
        (self.parse)(path)
    }
}

fn best_route_match<'a, R>(routes: &'a [R], absolute_path: &str) -> Option<(&'a R, Box<dyn Any>)>
where
    R: RegisteredRoute,
{
    routes
        .iter()
        .filter(|route| route.decl().pattern.matches_path(absolute_path))
        .max_by_key(|route| route.decl().pattern.precedence_key())
        .and_then(|route| route.parse(absolute_path).map(|parsed| (route, parsed)))
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

fn placeholder_size() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_FILE_SIZE_BYTES).expect("placeholder size must be non-zero")
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
