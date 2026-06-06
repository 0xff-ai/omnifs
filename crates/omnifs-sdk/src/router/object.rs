//! Object route registration, read path, and view-leaf expansion.

use crate::browse::{CachedCanonical, Effects, FileContent, ReadOutcome};
use crate::captures::{Captures, FromCaptures};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::{FileAttrs, FileProj, Size, Stability};
use crate::object::{FacetAxis, FacetMetadata, Key, Load, Object, ObjectShape, ProjectFn};
use crate::repr::{RenderSet, RenderTable};
use omnifs_core::ContentType;
use omnifs_core::path::{CaptureLocation, Pattern};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::handlers::{
    BoxedDirHandler, BoxedFileHandler, DirEntry, FileEntry, IntoDirHandler, IntoFileHandler,
    RouteValidator, captures_validator,
};
use super::pattern::parse_pattern;

type ObjectState<O> = <<O as Object>::Key as Key>::State;

/// A detached object subtree, replayable at multiple attach prefixes.
pub struct ObjectHandle<O: Object> {
    pub(super) template: &'static str,
    pub(super) shape: ObjectShape,
    pub(super) spec: std::rc::Rc<ObjectSpec<O>>,
}

/// Internal registration state built by an object block.
#[derive(Clone)]
pub(super) struct ObjectSpec<O: Object> {
    pub when: Option<fn(&O::Key) -> bool>,
    pub render_table: RenderTable,
    pub source_stem: &'static str,
    pub source_ext: &'static str,
    pub leaves: Vec<ObjectLeaf<O>>,
}

pub(super) enum ObjectLeaf<O: Object> {
    Representation {
        leaf_name: String,
        ct: ContentType,
        stability: Stability,
    },
    Projected {
        leaf_name: String,
        project: ProjectFn<O>,
        content_type: ContentType,
        lazy: bool,
        stability: Stability,
    },
    HandlerFile {
        suffix: String,
        handler: BoxedFileHandler<ObjectState<O>>,
        validator: RouteValidator,
    },
    HandlerDir {
        suffix: String,
        handler: BoxedDirHandler<ObjectState<O>>,
        validator: RouteValidator,
    },
}

impl<O: Object> Clone for ObjectLeaf<O> {
    fn clone(&self) -> Self {
        match self {
            Self::Representation {
                leaf_name,
                ct,
                stability,
            } => Self::Representation {
                leaf_name: leaf_name.clone(),
                ct: *ct,
                stability: *stability,
            },
            Self::Projected {
                leaf_name,
                project,
                content_type,
                lazy,
                stability,
            } => Self::Projected {
                leaf_name: leaf_name.clone(),
                project: *project,
                content_type: *content_type,
                lazy: *lazy,
                stability: *stability,
            },
            Self::HandlerFile {
                suffix,
                handler,
                validator,
            } => Self::HandlerFile {
                suffix: suffix.clone(),
                handler: Arc::clone(handler),
                validator: validator.clone(),
            },
            Self::HandlerDir {
                suffix,
                handler,
                validator,
            } => Self::HandlerDir {
                suffix: suffix.clone(),
                handler: Arc::clone(handler),
                validator: validator.clone(),
            },
        }
    }
}

/// Dir-shaped object block builder.
pub struct DirObjectBlock<O: Object> {
    template: &'static str,
    when: Option<fn(&O::Key) -> bool>,
    render_table: Option<RenderTable>,
    source_stem: Option<&'static str>,
    leaves: Vec<ObjectLeaf<O>>,
    leaf_claims: Vec<Pattern>,
    _marker: core::marker::PhantomData<O>,
}

/// File-shaped object block builder.
pub struct FileObjectBlock<O: Object> {
    inner: DirObjectBlock<O>,
}

pub struct FileLeafBuilder<'a, O: Object> {
    block: &'a mut DirObjectBlock<O>,
    name: &'static str,
}

pub struct DirLeafBuilder<'a, O: Object> {
    block: &'a mut DirObjectBlock<O>,
    name: &'static str,
}

impl<O: Object> DirObjectBlock<O> {
    fn new(template: &'static str) -> Result<Self> {
        parse_pattern(template)?;
        Ok(Self {
            template,
            when: None,
            render_table: None,
            source_stem: None,
            leaves: Vec::new(),
            leaf_claims: Vec::new(),
            _marker: core::marker::PhantomData,
        })
    }

    pub fn representations<R: RenderSet<O>>(
        &mut self,
        stem: &'static str,
        _renders: R,
    ) -> Result<&mut Self> {
        let source_ct = O::canonical_content_type();
        let ext = source_ct.extension().unwrap_or("raw");
        let source_leaf = format!("{stem}.{ext}");
        let source_pattern = parse_pattern(&format!(
            "{}/{}",
            self.template.trim_end_matches('/'),
            source_leaf
        ))?;
        self.leaf_claims.push(source_pattern);

        let mut renders = Vec::new();
        R::register(&mut renders);
        let table = RenderTable::build(source_ct, renders)?;
        for (ct, _) in &table.renders {
            let leaf = format!("{stem}.{}", ct.extension().unwrap_or("raw"));
            let pattern =
                parse_pattern(&format!("{}/{}", self.template.trim_end_matches('/'), leaf))?;
            self.leaf_claims.push(pattern);
            self.leaves.push(ObjectLeaf::Representation {
                leaf_name: leaf,
                ct: *ct,
                stability: O::default_stability(),
            });
        }

        self.render_table = Some(table);
        self.source_stem = Some(stem);
        self.leaves.push(ObjectLeaf::Representation {
            leaf_name: source_leaf,
            ct: source_ct,
            stability: O::default_stability(),
        });
        Ok(self)
    }

    pub fn file(&mut self, name: &'static str) -> FileLeafBuilder<'_, O> {
        FileLeafBuilder { block: self, name }
    }

    pub fn dir(&mut self, name: &'static str) -> DirLeafBuilder<'_, O> {
        DirLeafBuilder { block: self, name }
    }

    pub fn when(&mut self, pred: fn(&O::Key) -> bool) -> Result<&mut Self> {
        self.when = Some(pred);
        Ok(self)
    }

    fn finish(self, _shape: ObjectShape) -> Result<ObjectSpec<O>> {
        let render_table = self.render_table.ok_or_else(|| {
            ProviderError::invalid_input("object block requires representations(stem, ..)")
        })?;
        let source_stem = self.source_stem.ok_or_else(|| {
            ProviderError::invalid_input("object block requires representations(stem, ..)")
        })?;
        let source_ext = O::canonical_content_type().extension().unwrap_or("raw");
        Ok(ObjectSpec {
            when: self.when,
            render_table,
            source_stem,
            source_ext,
            leaves: self.leaves,
        })
    }
}

impl<O: Object> FileObjectBlock<O> {
    fn new(template: &'static str) -> Result<Self> {
        Ok(Self {
            inner: DirObjectBlock::new(template)?,
        })
    }

    pub fn representations<R: RenderSet<O>>(
        &mut self,
        stem: &'static str,
        renders: R,
    ) -> Result<&mut Self> {
        self.inner.representations(stem, renders)?;
        Ok(self)
    }

    pub fn when(&mut self, pred: fn(&O::Key) -> bool) -> Result<&mut Self> {
        self.inner.when(pred)?;
        Ok(self)
    }

    fn finish(self) -> Result<ObjectSpec<O>> {
        self.inner.finish(ObjectShape::File)
    }
}

impl<'a, O: Object> FileLeafBuilder<'a, O> {
    pub fn project(self, method: ProjectFn<O>) -> Result<&'a mut DirObjectBlock<O>> {
        let pattern = parse_pattern(&format!(
            "{}/{}",
            self.block.template.trim_end_matches('/'),
            self.name
        ))?;
        self.block.leaf_claims.push(pattern);
        self.block.leaves.push(ObjectLeaf::Projected {
            leaf_name: self.name.to_string(),
            project: method,
            content_type: ContentType::Custom("text/plain"),
            lazy: false,
            stability: O::default_stability(),
        });
        Ok(self.block)
    }

    pub fn handler<Marker, H>(self, h: H) -> Result<&'a mut DirObjectBlock<O>>
    where
        H: IntoFileHandler<ObjectState<O>, Marker>,
    {
        let suffix = self.name.to_string();
        let pattern = parse_pattern(&format!(
            "{}/{}",
            self.block.template.trim_end_matches('/'),
            suffix
        ))?;
        let (handler, validator) = h.into_file_handler();
        self.block.leaf_claims.push(pattern);
        self.block.leaves.push(ObjectLeaf::HandlerFile {
            suffix,
            handler,
            validator,
        });
        Ok(self.block)
    }

    pub fn lazy(self) -> Self {
        if let Some(ObjectLeaf::Projected { lazy, .. }) = self.block.leaves.last_mut() {
            *lazy = true;
        }
        self
    }

    pub fn immutable(self) -> Self {
        if let Some(ObjectLeaf::Projected { stability, .. }) = self.block.leaves.last_mut() {
            *stability = Stability::Immutable;
        }
        self
    }

    pub fn mutable(self) -> Self {
        if let Some(ObjectLeaf::Projected { stability, .. }) = self.block.leaves.last_mut() {
            *stability = Stability::Mutable;
        }
        self
    }

    pub fn volatile(self) -> Result<&'a mut DirObjectBlock<O>> {
        let is_handler = matches!(
            self.block.leaves.last(),
            Some(ObjectLeaf::HandlerFile { .. })
        );
        if !is_handler {
            return Err(ProviderError::invalid_input(
                ".volatile() is only valid on ranged .handler leaves",
            ));
        }
        Ok(self.block)
    }
}

impl<'a, O: Object> DirLeafBuilder<'a, O> {
    pub fn handler<Marker, H>(self, h: H) -> Result<&'a mut DirObjectBlock<O>>
    where
        H: IntoDirHandler<ObjectState<O>, Marker>,
    {
        let suffix = self.name.to_string();
        let pattern = parse_pattern(&format!(
            "{}/{}",
            self.block.template.trim_end_matches('/'),
            suffix
        ))?;
        let (handler, validator) = h.into_dir_handler();
        self.block.leaf_claims.push(pattern);
        self.block.leaves.push(ObjectLeaf::HandlerDir {
            suffix,
            handler,
            validator,
        });
        Ok(self.block)
    }
}

/// Define a detached dir-shaped object subtree.
pub fn object<O: Object>(
    template: &'static str,
    block: impl FnOnce(&mut DirObjectBlock<O>) -> Result<()>,
) -> Result<ObjectHandle<O>> {
    let mut builder = DirObjectBlock::new(template)?;
    block(&mut builder)?;
    let spec = builder.finish(ObjectShape::Dir)?;
    Ok(ObjectHandle {
        template,
        shape: ObjectShape::Dir,
        spec: std::rc::Rc::new(spec),
    })
}

pub(super) fn file_object<O: Object>(
    template: &'static str,
    block: impl FnOnce(&mut FileObjectBlock<O>) -> Result<()>,
) -> Result<ObjectHandle<O>> {
    let mut builder = FileObjectBlock::new(template)?;
    block(&mut builder)?;
    let spec = builder.finish()?;
    Ok(ObjectHandle {
        template,
        shape: ObjectShape::File,
        spec: std::rc::Rc::new(spec),
    })
}

pub(super) struct ObjectEntry<S> {
    pub pattern: Pattern,
    pub shape: ObjectShape,
    pub render_table: RenderTable,
    pub source_stem: String,
    pub source_ext: String,
    pub leaves: Vec<ListingLeaf>,
    pub read: BoxedObjectRead<S>,
    pub list: BoxedObjectList<S>,
    pub validator: RouteValidator,
}

pub(super) struct ListingLeaf {
    pub name: String,
    pub is_dir: bool,
}

impl ListingLeaf {
    fn handler(suffix: &str, is_dir: bool) -> Option<Self> {
        let pattern = parse_pattern(&format!("/{suffix}")).ok()?;
        let (name, has_child) = pattern.literal_child_after(&[])?;
        (!has_child).then_some(Self {
            name: name.to_string(),
            is_dir,
        })
    }
}

struct ObjectRoute<O: Object> {
    leaves: Vec<ObjectLeaf<O>>,
    render_table: RenderTable,
    facet_expansion: FacetExpansion,
    when: Option<fn(&O::Key) -> bool>,
}

impl<O: Object> Clone for ObjectRoute<O> {
    fn clone(&self) -> Self {
        Self {
            leaves: self.leaves.clone(),
            render_table: self.render_table.clone(),
            facet_expansion: self.facet_expansion.clone(),
            when: self.when,
        }
    }
}

impl<O: Object> ObjectRoute<O> {
    fn for_mount(spec: &ObjectSpec<O>, pattern: &Pattern) -> Result<Self>
    where
        O::Key: FacetMetadata,
    {
        Ok(Self {
            leaves: spec.leaves.clone(),
            render_table: spec.render_table.clone(),
            facet_expansion: FacetExpansion::for_pattern::<O::Key>(pattern)?,
            when: spec.when,
        })
    }

    fn read_handler<S>(self) -> BoxedObjectRead<S>
    where
        O: 'static,
        O::Key: Key<State = S> + FacetMetadata + 'static,
        S: 'static,
    {
        Box::new(
            move |cx: &Cx<S>,
                  caps: Captures,
                  target: ObjectReadTarget,
                  cached: Option<CachedCanonical>,
                  read_path: String| {
                let route = self.clone();
                Box::pin(async move { route.read(cx, caps, target, cached, read_path).await })
            },
        )
    }

    fn list_handler<S>(self) -> BoxedObjectList<S>
    where
        O: 'static,
        O::Key: Key<State = S> + FacetMetadata + 'static,
        S: 'static,
    {
        Box::new(move |cx: &Cx<S>, caps: Captures, list_path: String| {
            let route = self.clone();
            Box::pin(async move { route.list(cx, caps, list_path).await })
        })
    }

    async fn list<S>(&self, cx: &Cx<S>, caps: Captures, list_path: String) -> Result<Effects>
    where
        O::Key: Key<State = S> + FacetMetadata,
    {
        let key = O::Key::from_captures(&caps)?;
        if self.when.is_some_and(|pred| !pred(&key)) {
            return Err(ProviderError::not_found(format!(
                "object not found: {list_path}"
            )));
        }

        let since = cx.version().cloned();
        match key.load(cx, since).await? {
            Load::Fresh { value, canonical } => {
                let id = key.anchor();
                let mut effects = Effects::new();
                effects.canonical_store(
                    &id,
                    canonical.validator.clone(),
                    canonical.bytes,
                    self.view_leaves(&list_path)?,
                );
                self.project_eager_fields(&mut effects, &id, &value, &list_path)?;
                Ok(effects)
            },
            Load::Unchanged => Ok(Effects::new()),
            Load::NotFound => Err(ProviderError::not_found(format!(
                "object not found: {list_path}"
            ))),
        }
    }

    async fn read<S>(
        &self,
        cx: &Cx<S>,
        caps: Captures,
        target: ObjectReadTarget,
        cached: Option<CachedCanonical>,
        read_path: String,
    ) -> Result<ReadOutcome>
    where
        O::Key: Key<State = S> + FacetMetadata,
    {
        let key = O::Key::from_captures(&caps)?;
        if self.when.is_some_and(|pred| !pred(&key)) {
            return Ok(ReadOutcome::NotFound(None));
        }

        if let Some(ref push) = cached
            && push.matches_anchor(&key.anchor())
        {
            return serve_warm::<O>(target, &push.bytes, &self.render_table, &self.leaves);
        }

        let since = cached.as_ref().and_then(|p| p.validator.clone());
        match key.load(cx, since).await? {
            Load::Fresh { value, canonical } => {
                let id = key.anchor();
                let view_leaves = self.facet_expansion.expand_view_leaves(&read_path)?;
                let mut effects = Effects::new();
                effects.canonical_store(
                    &id,
                    canonical.validator.clone(),
                    canonical.bytes.clone(),
                    view_leaves,
                );
                serve_fresh::<O>(
                    &value,
                    target,
                    &canonical.bytes,
                    &self.render_table,
                    &self.leaves,
                    effects,
                )
            },
            Load::Unchanged => {
                let bytes = cached.as_ref().map(|p| p.bytes.as_slice()).ok_or_else(|| {
                    ProviderError::internal(
                        "Load::Unchanged returned without a host-pushed canonical",
                    )
                })?;
                serve_warm::<O>(target, bytes, &self.render_table, &self.leaves)
            },
            Load::NotFound => Ok(ReadOutcome::NotFound(Some(key.anchor()))),
        }
    }

    fn view_leaves(&self, list_path: &str) -> Result<Vec<String>> {
        let mut view_leaves = Vec::new();
        for leaf in &self.leaves {
            match leaf {
                ObjectLeaf::Representation { leaf_name, .. }
                | ObjectLeaf::Projected { leaf_name, .. } => {
                    let leaf_path = format!("{list_path}/{leaf_name}");
                    view_leaves.extend(self.facet_expansion.expand_view_leaves(&leaf_path)?);
                },
                ObjectLeaf::HandlerFile { .. } | ObjectLeaf::HandlerDir { .. } => {},
            }
        }
        Ok(view_leaves)
    }

    fn project_eager_fields(
        &self,
        effects: &mut Effects,
        id: &crate::identity::LogicalId,
        value: &O,
        list_path: &str,
    ) -> Result<()> {
        for leaf in &self.leaves {
            let ObjectLeaf::Projected {
                leaf_name,
                project,
                lazy,
                stability,
                ..
            } = leaf
            else {
                continue;
            };
            if *lazy {
                continue;
            }
            let content = project(value)?;
            let bytes = content.content().ok_or_else(|| {
                ProviderError::internal(format!(
                    "projected object leaf {leaf_name:?} cannot preload non-inline bytes"
                ))
            })?;
            let mut file = FileProj::inline(bytes.to_vec(), *stability, None);
            if let Some(content_type) = content.content_type() {
                file = file.with_content_type(content_type);
            }
            effects.project_file_with_id(format!("{list_path}/{leaf_name}"), Some(id), file)?;
        }
        Ok(())
    }
}

pub(super) struct MountedObject<S> {
    pub entry: ObjectEntry<S>,
    pub claims: Vec<Pattern>,
    pub handler_files: Vec<FileEntry<S>>,
    pub handler_dirs: Vec<DirEntry<S>>,
}

type BoxedObjectRead<S> = Box<
    dyn for<'a> Fn(
        &'a Cx<S>,
        Captures,
        ObjectReadTarget,
        Option<CachedCanonical>,
        String,
    ) -> Pin<Box<dyn Future<Output = Result<ReadOutcome>> + 'a>>,
>;

type BoxedObjectList<S> = Box<
    dyn for<'a> Fn(
        &'a Cx<S>,
        Captures,
        String,
    ) -> Pin<Box<dyn Future<Output = Result<Effects>> + 'a>>,
>;

pub(super) enum ObjectReadTarget {
    Representation(ContentType),
    Projected(String),
}

fn mounted_leaf_claims<O: Object>(
    spec: &ObjectSpec<O>,
    mount_template: &str,
) -> Result<Vec<Pattern>> {
    let mount = mount_template.trim_end_matches('/');
    let mut claims = Vec::new();
    for leaf in &spec.leaves {
        // Handler leaves get their leaf claim where their FileEntry/DirEntry is
        // built in `mount_object`; claiming them here too would self-overlap.
        let suffix = match leaf {
            ObjectLeaf::Representation { leaf_name, .. }
            | ObjectLeaf::Projected { leaf_name, .. } => leaf_name.as_str(),
            ObjectLeaf::HandlerFile { .. } | ObjectLeaf::HandlerDir { .. } => continue,
        };
        claims.push(parse_pattern(&format!("{mount}/{suffix}"))?);
    }
    Ok(claims)
}

pub(super) fn mount_object<O, S>(
    pattern: &Pattern,
    shape: ObjectShape,
    spec: &ObjectSpec<O>,
    combined_template: &str,
) -> Result<MountedObject<S>>
where
    O: Object + 'static,
    O::Key: Key<State = S> + FacetMetadata + 'static,
    S: 'static,
{
    let listing_leaves: Vec<ListingLeaf> = spec
        .leaves
        .iter()
        .filter_map(|leaf| match leaf {
            ObjectLeaf::Representation { leaf_name, .. }
            | ObjectLeaf::Projected { leaf_name, .. } => Some(ListingLeaf {
                name: leaf_name.clone(),
                is_dir: false,
            }),
            ObjectLeaf::HandlerFile { suffix, .. } => ListingLeaf::handler(suffix, false),
            ObjectLeaf::HandlerDir { suffix, .. } => ListingLeaf::handler(suffix, true),
        })
        .collect();

    let mut leaf_claims = mounted_leaf_claims(spec, combined_template)?;
    leaf_claims.push(pattern.clone());

    let mut handler_files = Vec::new();
    let mut handler_dirs = Vec::new();
    for leaf in &spec.leaves {
        match leaf {
            ObjectLeaf::HandlerFile {
                suffix,
                handler,
                validator,
            } => {
                let template = format!("{combined_template}/{suffix}");
                if let Ok(child_pattern) = parse_pattern(&template) {
                    leaf_claims.push(child_pattern.clone());
                    handler_files.push(FileEntry {
                        pattern: child_pattern,
                        handler: handler.clone(),
                        validator: validator.clone(),
                    });
                }
            },
            ObjectLeaf::HandlerDir {
                suffix,
                handler,
                validator,
            } => {
                let template = format!("{combined_template}/{suffix}");
                if let Ok(child_pattern) = parse_pattern(&template) {
                    leaf_claims.push(child_pattern.clone());
                    handler_dirs.push(DirEntry {
                        pattern: child_pattern,
                        handler: handler.clone(),
                        validator: validator.clone(),
                    });
                }
            },
            _ => {},
        }
    }

    let route = ObjectRoute::for_mount(spec, pattern)?;

    let entry = ObjectEntry {
        pattern: pattern.clone(),
        shape,
        render_table: spec.render_table.clone(),
        source_stem: spec.source_stem.to_string(),
        source_ext: spec.source_ext.to_string(),
        leaves: listing_leaves,
        read: route.clone().read_handler::<S>(),
        list: route.list_handler::<S>(),
        validator: captures_validator::<O::Key>(),
    };

    Ok(MountedObject {
        entry,
        claims: leaf_claims,
        handler_files,
        handler_dirs,
    })
}

fn serve_warm<O: Object>(
    target: ObjectReadTarget,
    bytes: &[u8],
    render_table: &RenderTable,
    leaves: &[ObjectLeaf<O>],
) -> Result<ReadOutcome> {
    serve_from_canonical::<O>(target, bytes, render_table, leaves, Effects::new())
}

fn serve_fresh<O: Object>(
    value: &O,
    target: ObjectReadTarget,
    bytes: &[u8],
    render_table: &RenderTable,
    leaves: &[ObjectLeaf<O>],
    effects: Effects,
) -> Result<ReadOutcome> {
    match target {
        ObjectReadTarget::Projected(name) => {
            for leaf in leaves {
                if let ObjectLeaf::Projected {
                    leaf_name,
                    project,
                    content_type,
                    stability,
                    ..
                } = leaf
                    && leaf_name == &name
                {
                    let mut content = project(value)?;
                    let size = content_size(&content);
                    content = content
                        .with_content_type(*content_type)
                        .with_attrs(FileAttrs::new(Size::Exact(size), *stability));
                    return Ok(ReadOutcome::Found(content.with_effects(effects)));
                }
            }
            Err(ProviderError::not_found(format!("field {name} not found")))
        },
        ObjectReadTarget::Representation(ct) => serve_from_canonical::<O>(
            ObjectReadTarget::Representation(ct),
            bytes,
            render_table,
            leaves,
            effects,
        ),
    }
}

fn serve_from_canonical<O: Object>(
    target: ObjectReadTarget,
    bytes: &[u8],
    render_table: &RenderTable,
    leaves: &[ObjectLeaf<O>],
    effects: Effects,
) -> Result<ReadOutcome> {
    match target {
        ObjectReadTarget::Representation(ct) => {
            if ct == render_table.source_ct {
                return Ok(ReadOutcome::Found(
                    FileContent::canonical(canonical_attrs()).with_effects(effects),
                ));
            }
            let rendered = render_table.serve(ct, bytes)?;
            Ok(ReadOutcome::Found(
                body_file_content(rendered, ct).with_effects(effects),
            ))
        },
        ObjectReadTarget::Projected(name) => {
            let value = O::parse_canonical(bytes)?;
            for leaf in leaves {
                if let ObjectLeaf::Projected {
                    leaf_name,
                    project,
                    content_type,
                    stability,
                    ..
                } = leaf
                    && leaf_name == &name
                {
                    let mut content = project(&value)?;
                    let size = content_size(&content);
                    content = content
                        .with_content_type(*content_type)
                        .with_attrs(FileAttrs::new(Size::Exact(size), *stability));
                    return Ok(ReadOutcome::Found(content.with_effects(effects)));
                }
            }
            Err(ProviderError::not_found(format!("field {name} not found")))
        },
    }
}

#[derive(Clone, Debug)]
pub(super) struct FacetExpansion {
    axes: Vec<FacetExpansionAxis>,
}

impl FacetExpansion {
    pub(super) fn for_pattern<K: FacetMetadata>(pattern: &Pattern) -> Result<Self> {
        let axes = K::facet_axes()
            .iter()
            .map(|axis| FacetExpansionAxis::for_pattern(pattern, axis))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { axes })
    }

    pub(super) fn expand_view_leaves(&self, read_path: &str) -> Result<Vec<String>> {
        if self.axes.is_empty() {
            return Ok(vec![read_path.to_string()]);
        }

        let mut paths = vec![read_path.to_string()];
        for axis in &self.axes {
            let mut next = Vec::new();
            for path in &paths {
                for choice in axis.choices {
                    next.push(axis.substitute(path, choice)?);
                }
            }
            if !next.is_empty() {
                paths = next;
            }
        }
        Ok(paths)
    }
}

#[derive(Clone, Debug)]
struct FacetExpansionAxis {
    location: CaptureLocation,
    choices: &'static [&'static str],
}

impl FacetExpansionAxis {
    fn for_pattern(pattern: &Pattern, axis: &FacetAxis) -> Result<Self> {
        let location = pattern.capture_location(axis.capture_name).ok_or_else(|| {
            ProviderError::invalid_input(format!(
                "facet capture {:?} is not present in object route",
                axis.capture_name
            ))
        })?;
        Ok(Self {
            location,
            choices: axis.choices,
        })
    }

    fn substitute(&self, path: &str, choice: &str) -> Result<String> {
        let offset = usize::from(path.starts_with('/'));
        let path_index = self.location.segment_index() + offset;
        let mut segments = path.split('/').map(str::to_string).collect::<Vec<_>>();
        let Some(segment) = segments.get_mut(path_index) else {
            return Err(ProviderError::internal(format!(
                "path {path:?} is missing facet segment at index {}",
                self.location.segment_index()
            )));
        };
        *segment = self.location.render_segment(choice);
        Ok(segments.join("/"))
    }
}

fn content_size(content: &FileContent) -> u64 {
    content
        .content()
        .map_or(0, |b| u64::try_from(b.len()).unwrap_or(u64::MAX))
}

pub(super) fn canonical_attrs() -> FileAttrs {
    FileAttrs::new(Size::Unknown, Stability::Immutable)
}

pub(super) fn body_file_content(bytes: Vec<u8>, ct: ContentType) -> FileContent {
    FileContent::new(bytes).with_content_type(ct)
}
