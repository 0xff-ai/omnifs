//! Object route registration, read path, and view-leaf expansion.
//!
//! An object route binds a typed [`Object`] to a template: the key parsed
//! from the captures both identifies the resource ([`Key::anchor`]) and
//! loads it ([`Key::load`]); there is no separate fetcher. The block passed
//! to `r.object::<O>(..)` declares what the anchor directory contains:
//!
//! - [`DirObjectBlock::representations`]: the canonical source leaf plus one
//!   rendered leaf per render set entry (`item.json`, `item.md`); mandatory.
//! - [`DirObjectBlock::file`] with [`FileLeafBuilder::project`]: a field leaf
//!   computed from the loaded value (`title`, `state`).
//!
//! The cache contract (the host owns all caching; the SDK only emits
//! effects): every fresh [`Key::load`] produces a `canonical-store` effect
//! carrying the verbatim upstream bytes, the validator, and the expanded
//! view-leaf paths that map back to the object's logical id. On a later
//! read the host pushes those bytes back as a [`CachedCanonical`] and the
//! SDK re-renders without an upstream call. Facets (identity-neutral
//! captures with finite choices) multiply the view leaves, so loading
//! `/issues/open/7/title` also teaches the host `/issues/closed/7/title`
//! and `/issues/all/7/title`.

use super::pattern::{CaptureLocation, Pattern};
use crate::browse::{CachedCanonical, Effects, FileContent, ReadOutcome};
use crate::captures::{Captures, FromCaptures};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::{FileAttrs, ProjBytes, Size, Stability, VersionToken};
use crate::object::{FacetAxis, FacetMetadata, Key, Load, Object, ProjectFn};
use crate::projection::FileProjection;
use crate::repr::{RenderSet, RenderTable};
use omnifs_core::ContentType;
use std::future::Future;
use std::pin::Pin;

use super::handlers::{RouteValidator, captures_validator};
use super::pattern::parse_pattern;

/// A detached object subtree, replayable at multiple attach prefixes.
///
/// Built by [`object()`]; mounted with
/// [`Router::attach`](super::Router::attach). The spec is shared by `Rc`, so
/// attaching the same handle twice replays one definition at two prefixes,
/// each with its own leaf claims.
pub struct ObjectHandle<O: Object> {
    pub(super) template: &'static str,
    pub(super) spec: std::rc::Rc<ObjectSpec<O>>,
}

/// Internal registration state built by an object block, independent of any
/// attach prefix; [`mount_object`] specializes it per mount.
#[derive(Clone)]
pub(super) struct ObjectSpec<O: Object> {
    pub when: Option<fn(&O::Key) -> bool>,
    pub stability: fn(&O::Key) -> Stability,
    pub render_table: RenderTable,
    pub source_stem: &'static str,
    pub source_ext: &'static str,
    pub leaves: Vec<ObjectLeaf<O>>,
}

/// One declared child of an object anchor.
///
/// `Representation` and `Projected` leaves are served through the object
/// read path (rendered or projected from canonical bytes) and contribute
/// view leaves to the canonical-store effect.
pub(super) enum ObjectLeaf<O: Object> {
    /// A `stem.ext` leaf: the canonical bytes themselves or a registered
    /// render of them.
    Representation { leaf_name: String, ct: ContentType },
    /// A field leaf computed from the parsed object value. `lazy` excludes it
    /// from listing-time eager preloads; reads still serve it.
    Projected {
        leaf_name: String,
        project: ProjectFn<O>,
        lazy: bool,
    },
}

impl<O: Object> Clone for ObjectLeaf<O> {
    fn clone(&self) -> Self {
        match self {
            Self::Representation { leaf_name, ct } => Self::Representation {
                leaf_name: leaf_name.clone(),
                ct: *ct,
            },
            Self::Projected {
                leaf_name,
                project,
                lazy,
            } => Self::Projected {
                leaf_name: leaf_name.clone(),
                project: *project,
                lazy: *lazy,
            },
        }
    }
}

/// Dir-shaped object block builder: the anchor is a directory whose children
/// are the declared representations and projected fields.
/// [`Self::representations`] must be called or the block fails to finish.
pub struct DirObjectBlock<O: Object> {
    template: &'static str,
    when: Option<fn(&O::Key) -> bool>,
    stability: Option<fn(&O::Key) -> Stability>,
    render_table: Option<RenderTable>,
    source_stem: Option<&'static str>,
    leaves: Vec<ObjectLeaf<O>>,
    leaf_claims: Vec<Pattern>,
    _marker: core::marker::PhantomData<O>,
}

/// A pending file leaf named in [`DirObjectBlock::file`]; finish with
/// [`Self::project`] (a field computed from the object value).
pub struct FileLeafBuilder<'a, O: Object> {
    block: &'a mut DirObjectBlock<O>,
    name: &'static str,
    lazy: bool,
}

impl<O: Object> DirObjectBlock<O> {
    fn new(template: &'static str) -> Result<Self> {
        parse_pattern(template)?;
        Ok(Self {
            template,
            when: None,
            stability: None,
            render_table: None,
            source_stem: None,
            leaves: Vec::new(),
            leaf_claims: Vec::new(),
            _marker: core::marker::PhantomData,
        })
    }

    /// Declare the anchor's representation leaves; mandatory, once per block.
    ///
    /// Registers `stem.<ext>` for the canonical content type (e.g.
    /// `item.json` when [`Object::canonical_content_type`] is JSON) plus one
    /// `stem.<ext>` per entry in the render set `R` (e.g. `(Markdown,)`
    /// adds `item.md`; `()` adds none). Each leaf is claimed against
    /// [`Router::seal`](super::Router::seal). All representation leaves
    /// carry the object's declared [`Self::stability`] (a rendering inherits
    /// its canonical's); renders are recomputed from cached canonical bytes,
    /// never fetched separately.
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
            });
        }

        self.render_table = Some(table);
        self.source_stem = Some(stem);
        self.leaves.push(ObjectLeaf::Representation {
            leaf_name: source_leaf,
            ct: source_ct,
        });
        Ok(self)
    }

    /// Begin a projected file leaf under the anchor. `name` must be a single
    /// literal leaf name.
    pub fn file(&mut self, name: &'static str) -> FileLeafBuilder<'_, O> {
        FileLeafBuilder {
            block: self,
            name,
            lazy: false,
        }
    }

    /// Gate the whole object on a key predicate. A key that fails the
    /// predicate behaves as not-found for both listing and reads; no load is
    /// attempted.
    pub fn when(&mut self, pred: fn(&O::Key) -> bool) -> Result<&mut Self> {
        self.when = Some(pred);
        Ok(self)
    }

    /// Declare the object's [`Stability`] as a function of its key, shared by
    /// the canonical and every leaf derived from it (a rendering inherits the
    /// canonical's). A pinned identity is `Stable`, a "latest" alias is
    /// `Dynamic`; e.g. `o.stability(|key| if key.numbered() { Stable } else {
    /// Dynamic })`. For a stability that is the same for every key, prefer the
    /// [`Self::stable`] / [`Self::dynamic`] / [`Self::live`] shorthands.
    /// Mandatory, once per block; the block fails to finish otherwise.
    pub fn stability(&mut self, f: fn(&O::Key) -> Stability) -> &mut Self {
        self.stability = Some(f);
        self
    }

    /// Shorthand for `stability(|_| Stability::Stable)`: the object's bytes
    /// never change for any key (a content-addressed or versioned identity).
    pub fn stable(&mut self) -> &mut Self {
        self.stability(|_| Stability::Stable)
    }

    /// Shorthand for `stability(|_| Stability::Dynamic)`: each read is a
    /// consistent snapshot, but later reads may differ.
    pub fn dynamic(&mut self) -> &mut Self {
        self.stability(|_| Stability::Dynamic)
    }

    /// Shorthand for `stability(|_| Stability::Live)`: a moving target that
    /// may change while being observed.
    pub fn live(&mut self) -> &mut Self {
        self.stability(|_| Stability::Live)
    }

    fn finish(self) -> Result<ObjectSpec<O>> {
        let render_table = self.render_table.ok_or_else(|| {
            ProviderError::invalid_input("object block requires representations(stem, ..)")
        })?;
        let source_stem = self.source_stem.ok_or_else(|| {
            ProviderError::invalid_input("object block requires representations(stem, ..)")
        })?;
        let source_ext = O::canonical_content_type().extension().unwrap_or("raw");
        let stability = self.stability.ok_or_else(|| {
            ProviderError::invalid_input(
                "object block requires a stability declaration: stability(|key| ..) or stable()/dynamic()/live()",
            )
        })?;
        Ok(ObjectSpec {
            when: self.when,
            stability,
            render_table,
            source_stem,
            source_ext,
            leaves: self.leaves,
        })
    }
}

impl<'a, O: Object> FileLeafBuilder<'a, O> {
    /// Register a projected field leaf: `method` maps the loaded object value
    /// and route key to the leaf's bytes, so reads can be served from cached
    /// canonical bytes with no upstream call. Its stability is the object's
    /// declared stability for the key (a projected field inherits the
    /// canonical's); the leaf is eager (preloaded into the view cache when the
    /// anchor is listed) unless flagged lazy. Eager projections must produce
    /// inline bytes; listing fails otherwise.
    pub fn project(
        self,
        method: fn(&O, &O::Key) -> Result<FileProjection>,
    ) -> Result<&'a mut DirObjectBlock<O>> {
        let pattern = parse_pattern(&format!(
            "{}/{}",
            self.block.template.trim_end_matches('/'),
            self.name
        ))?;
        self.block.leaf_claims.push(pattern);
        self.block.leaves.push(ObjectLeaf::Projected {
            leaf_name: self.name.to_string(),
            project: method,
            lazy: self.lazy,
        });
        Ok(self.block)
    }

    /// Exclude this projected leaf from listing-time eager preloads; reads
    /// still serve it from canonical bytes. Use for large fields (an issue
    /// body) where preloading every list row would bloat the view cache.
    ///
    /// This is a modifier for the pending [`Self::project`] leaf:
    /// `o.file("body").lazy().project(|issue, _key| issue.body())?`.
    #[must_use]
    pub fn lazy(mut self) -> Self {
        self.lazy = true;
        self
    }
}

/// Define a detached dir-shaped object subtree, to be mounted later with
/// [`Router::attach`](super::Router::attach). `template` must be absolute;
/// the block must call [`DirObjectBlock::representations`].
pub fn object<O: Object>(
    template: &'static str,
    block: impl FnOnce(&mut DirObjectBlock<O>) -> Result<()>,
) -> Result<ObjectHandle<O>> {
    let mut builder = DirObjectBlock::new(template)?;
    block(&mut builder)?;
    let spec = builder.finish()?;
    Ok(ObjectHandle {
        template,
        spec: std::rc::Rc::new(spec),
    })
}

/// The mounted, type-erased object route the dispatch tables hold: the
/// anchor pattern, the leaf names to list, and boxed read/list closures that
/// re-instantiate the typed `ObjectRoute` per call.
pub(super) struct ObjectEntry<S> {
    pub pattern: Pattern,
    pub render_table: RenderTable,
    pub source_stem: String,
    pub source_ext: String,
    pub leaves: Vec<ListingLeaf>,
    pub read: BoxedObjectRead<S>,
    pub list: BoxedObjectList<S>,
    pub validator: RouteValidator,
}

/// A child name an object anchor lists, precomputed at mount time.
pub(super) struct ListingLeaf {
    pub name: String,
}

/// The typed runtime side of a mounted object: everything `read`/`list`
/// need, cloneable so each boxed call can move an owned copy into its
/// future.
struct ObjectRoute<O: Object> {
    leaves: Vec<ObjectLeaf<O>>,
    stability: fn(&O::Key) -> Stability,
    render_table: RenderTable,
    facet_expansion: FacetExpansion,
    when: Option<fn(&O::Key) -> bool>,
}

impl<O: Object> Clone for ObjectRoute<O> {
    fn clone(&self) -> Self {
        Self {
            leaves: self.leaves.clone(),
            stability: self.stability,
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
            stability: spec.stability,
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

    /// The anchor-listing side effects. Listing entries come from the
    /// precomputed [`ListingLeaf`] names in dispatch; this loads the object
    /// (conditionally, using the host-pushed validator from `cx.version()`)
    /// and emits the canonical-store effect plus eager field preloads. A
    /// fresh load teaches the host every view leaf; `Unchanged` emits
    /// nothing; `NotFound` makes the whole anchor not-found.
    async fn list<S>(&self, cx: &Cx<S>, caps: Captures, list_path: String) -> Result<ObjectListing>
    where
        O::Key: Key<State = S> + FacetMetadata,
    {
        let key = O::Key::from_captures(&caps)?;
        if self.when.is_some_and(|pred| !pred(&key)) {
            return Err(ProviderError::not_found(format!(
                "object not found: {list_path}"
            )));
        }
        let stability = (self.stability)(&key);

        let since = cx.version().cloned();
        let (value, canonical, extra_effects) = match key.load(cx, since).await? {
            Load::Fresh {
                value,
                canonical,
                effects,
            } => (value, canonical, effects),
            Load::Unchanged => {
                return Ok(ObjectListing {
                    effects: Effects::new(),
                    source: None,
                });
            },
            Load::NotFound => {
                return Err(ProviderError::not_found(format!(
                    "object not found: {list_path}"
                )));
            },
        };
        // The source representation leaf is the verbatim canonical bytes, so
        // its size is known here without a read; stamp it onto the listing.
        let source = SourceLeafAttrs {
            len: canonical.bytes.len() as u64,
            validator: canonical.validator.clone(),
            stability,
        };
        let id = key.anchor();
        let mut effects = Effects::new();
        effects.canonical_store(
            &id,
            canonical.validator.clone(),
            canonical.bytes,
            self.view_leaves(&list_path)?,
        );
        effects.extend(extra_effects);
        self.project_eager_fields(&mut effects, &id, &value, &key, &list_path, stability)?;
        Ok(ObjectListing {
            effects,
            source: Some(source),
        })
    }

    /// The object read path, in priority order:
    ///
    /// 1. Warm: the host pushed cached canonical bytes and the SDK verified
    ///    the pushed id against the route-derived [`Key::anchor`]; render
    ///    with no upstream call and no new effects. A mismatched push is not
    ///    served warm; dispatch falls through to a load.
    /// 2. Fresh: [`Key::load`] with the pushed validator as `since`; emits a
    ///    canonical-store effect with facet-expanded view leaves for the
    ///    read path, then serves the requested representation or field.
    /// 3. `Load::Unchanged`: re-renders from the pushed bytes; it is an
    ///    internal error for a load to claim unchanged when the host pushed
    ///    nothing to be unchanged against.
    /// 4. `Load::NotFound`: reports not-found tagged with the anchor id, so
    ///    the host can key the negative entry to the object and clear it on
    ///    the object's next invalidation.
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

        let stability = (self.stability)(&key);

        if let Some(ref push) = cached
            && push.matches_anchor(&key.anchor())
        {
            return serve_warm::<O>(
                &key,
                target,
                &push.bytes,
                push.validator.clone(),
                ServeCtx {
                    render_table: &self.render_table,
                    leaves: &self.leaves,
                    stability,
                },
            );
        }

        let since = cached.as_ref().and_then(|p| p.validator.clone());
        let (value, canonical, extra_effects) = match key.load(cx, since).await? {
            Load::Fresh {
                value,
                canonical,
                effects,
            } => (value, canonical, effects),
            Load::Unchanged => {
                let bytes = cached.as_ref().map(|p| p.bytes.as_slice()).ok_or_else(|| {
                    ProviderError::internal(
                        "Load::Unchanged returned without a host-pushed canonical",
                    )
                })?;
                let validator = cached.as_ref().and_then(|p| p.validator.clone());
                return serve_warm::<O>(
                    &key,
                    target,
                    bytes,
                    validator,
                    ServeCtx {
                        render_table: &self.render_table,
                        leaves: &self.leaves,
                        stability,
                    },
                );
            },
            Load::NotFound => return Ok(ReadOutcome::NotFound(Some(key.anchor()))),
        };
        let id = key.anchor();
        let view_leaves = self.facet_expansion.expand_view_leaves(&read_path)?;
        let mut effects = Effects::new();
        effects.canonical_store(
            &id,
            canonical.validator.clone(),
            canonical.bytes.clone(),
            view_leaves,
        );
        effects.extend(extra_effects);
        serve_fresh::<O>(
            &value,
            &key,
            target,
            &canonical.bytes,
            canonical.validator.clone(),
            ServeCtx {
                render_table: &self.render_table,
                leaves: &self.leaves,
                stability,
            },
            effects,
        )
    }

    /// Every full path that maps to this object's canonical bytes: each
    /// representation and projected leaf under the anchor, multiplied across
    /// facet choices.
    fn view_leaves(&self, list_path: &str) -> Result<Vec<String>> {
        let mut view_leaves = Vec::new();
        for leaf in &self.leaves {
            match leaf {
                ObjectLeaf::Representation { leaf_name, .. }
                | ObjectLeaf::Projected { leaf_name, .. } => {
                    let leaf_path = format!("{list_path}/{leaf_name}");
                    view_leaves.extend(self.facet_expansion.expand_view_leaves(&leaf_path)?);
                },
            }
        }
        Ok(view_leaves)
    }

    /// Materialize every non-lazy projected leaf into `fs` effects at listing
    /// time, tagged with the object id so leaf invalidation cascades. This is
    /// the preload discipline applied to objects: the value is already in
    /// hand, so its cheap fields ship now instead of forcing per-leaf reads.
    /// Every leaf carries the object's `stability` (the rendering inherits the
    /// canonical's). Errors when a projection yields non-inline bytes; eager
    /// preloads must be inline.
    fn project_eager_fields(
        &self,
        effects: &mut Effects,
        id: &crate::identity::LogicalId,
        value: &O,
        key: &O::Key,
        list_path: &str,
        stability: Stability,
    ) -> Result<()> {
        for leaf in &self.leaves {
            let ObjectLeaf::Projected {
                leaf_name,
                project,
                lazy,
                ..
            } = leaf
            else {
                continue;
            };
            if *lazy {
                continue;
            }
            let projection = project(value, key)?;
            let mut file = projection.as_file_proj().ok_or_else(|| {
                ProviderError::internal(format!(
                    "projected object leaf {leaf_name:?} cannot preload non-inline bytes"
                ))
            })?;
            if !matches!(file.bytes, ProjBytes::Inline(_)) {
                return Err(ProviderError::internal(format!(
                    "projected object leaf {leaf_name:?} cannot preload non-inline bytes"
                )));
            }
            file.attrs = FileAttrs::new(file.attrs.size, stability);
            effects.project_file_with_id(format!("{list_path}/{leaf_name}"), Some(id), file)?;
        }
        Ok(())
    }
}

/// What mounting an object spec yields: the dispatchable entry and the leaf
/// claims to feed [`Router::seal`](super::Router::seal).
pub(super) struct MountedObject<S> {
    pub entry: ObjectEntry<S>,
    pub claims: Vec<Pattern>,
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
    ) -> Pin<Box<dyn Future<Output = Result<ObjectListing>> + 'a>>,
>;

/// What an anchor's list dispatch needs: the load's effects, plus the exact
/// attrs of the verbatim source representation leaf (its bytes are the loaded
/// canonical, so its size is known without a read). The dispatch stamps the
/// source leaf with these so a cold `ls -l` reports the real size; rendered
/// leaves stay size-unknown until read (their length needs a render).
pub(super) struct ObjectListing {
    pub effects: Effects,
    /// `None` on an `Unchanged` load: the host serves the cached dirent, which
    /// a prior fresh listing already stamped.
    pub source: Option<SourceLeafAttrs>,
}

pub(super) struct SourceLeafAttrs {
    pub len: u64,
    pub validator: Option<VersionToken>,
    pub stability: Stability,
}

/// Which child of the anchor a read addresses: a representation by content
/// type (dispatch resolves the `stem.ext` leaf name through the render
/// table), or a projected field by leaf name.
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
        let suffix = match leaf {
            ObjectLeaf::Representation { leaf_name, .. }
            | ObjectLeaf::Projected { leaf_name, .. } => leaf_name.as_str(),
        };
        claims.push(parse_pattern(&format!("{mount}/{suffix}"))?);
    }
    Ok(claims)
}

/// Specialize an [`ObjectSpec`] at a concrete mount pattern: precompute the
/// listing names, build the per-mount facet expansion, and collect every leaf
/// claim for the seal check.
pub(super) fn mount_object<O, S>(
    pattern: &Pattern,
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
        .map(|leaf| match leaf {
            ObjectLeaf::Representation { leaf_name, .. }
            | ObjectLeaf::Projected { leaf_name, .. } => ListingLeaf {
                name: leaf_name.clone(),
            },
        })
        .collect();

    let mut leaf_claims = mounted_leaf_claims(spec, combined_template)?;
    leaf_claims.push(pattern.clone());

    let route = ObjectRoute::for_mount(spec, pattern)?;

    let entry = ObjectEntry {
        pattern: pattern.clone(),
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
    })
}

/// The object projection context shared by every serve path: the route-owned
/// render table and leaf set, plus the key-resolved `stability` that every
/// leaf inherits. Grouped so the serve helpers keep a sane argument count.
struct ServeCtx<'a, O: Object> {
    render_table: &'a RenderTable,
    leaves: &'a [ObjectLeaf<O>],
    stability: Stability,
}

// All fields are `Copy` (two shared borrows and a `Stability`); a manual impl
// keeps `ServeCtx` `Copy` without a derive's spurious `O: Copy` bound.
impl<O: Object> Clone for ServeCtx<'_, O> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<O: Object> Copy for ServeCtx<'_, O> {}

/// Serve from host-pushed canonical bytes with no effects: the host already
/// owns these bytes, so re-storing them would be redundant.
fn serve_warm<O: Object>(
    key: &O::Key,
    target: ObjectReadTarget,
    bytes: &[u8],
    validator: Option<VersionToken>,
    ctx: ServeCtx<'_, O>,
) -> Result<ReadOutcome> {
    serve_from_canonical::<O>(key, target, bytes, validator, ctx, Effects::new())
}

/// Serve right after a fresh load, attaching the canonical-store effects.
/// A projected target uses the already-parsed value (no re-parse of the
/// bytes); representations render from the canonical bytes. Every leaf
/// carries the object's `stability` (a rendering inherits the canonical's).
fn serve_fresh<O: Object>(
    value: &O,
    key: &O::Key,
    target: ObjectReadTarget,
    bytes: &[u8],
    validator: Option<VersionToken>,
    ctx: ServeCtx<'_, O>,
    effects: Effects,
) -> Result<ReadOutcome> {
    match target {
        ObjectReadTarget::Projected(name) => serve_projected(value, key, &name, ctx, effects),
        ObjectReadTarget::Representation(ct) => serve_from_canonical::<O>(
            key,
            ObjectReadTarget::Representation(ct),
            bytes,
            validator,
            ctx,
            effects,
        ),
    }
}

/// Serve any target from canonical bytes. The source content type answers
/// with the `byte-source::canonical` identity terminal (the host already
/// holds the bytes; they are not echoed back); other representations render
/// through the table; a projected field re-parses the canonical and runs
/// its projection. Every target carries the object's `stability`, resolved
/// once from the key by the caller (a rendering inherits the canonical's).
fn serve_from_canonical<O: Object>(
    key: &O::Key,
    target: ObjectReadTarget,
    bytes: &[u8],
    validator: Option<VersionToken>,
    ctx: ServeCtx<'_, O>,
    effects: Effects,
) -> Result<ReadOutcome> {
    match target {
        ObjectReadTarget::Representation(ct) => {
            if ct == ctx.render_table.source_ct {
                return Ok(ReadOutcome::Found(
                    FileContent::canonical(representation_attrs(
                        Size::Unknown,
                        ctx.stability,
                        validator,
                    ))
                    .with_effects(effects),
                ));
            }
            let rendered = ctx.render_table.serve(ct, bytes)?;
            Ok(ReadOutcome::Found(
                body_file_content(rendered, ct, ctx.stability, validator).with_effects(effects),
            ))
        },
        ObjectReadTarget::Projected(name) => {
            let value = O::parse_canonical(bytes)?;
            serve_projected(&value, key, &name, ctx, effects)
        },
    }
}

/// Serve a projected field leaf by name from an already-parsed object value,
/// stamping the object's `stability`. Shared by the warm/fresh path (value in
/// hand) and the canonical re-render path (value parsed from pushed bytes).
fn serve_projected<O: Object>(
    value: &O,
    key: &O::Key,
    name: &str,
    ctx: ServeCtx<'_, O>,
    effects: Effects,
) -> Result<ReadOutcome> {
    for leaf in ctx.leaves {
        if let ObjectLeaf::Projected {
            leaf_name, project, ..
        } = leaf
            && leaf_name == name
        {
            let content = project(value, key)?.into_browse_content()?;
            let size = content_size(&content);
            let content = content.with_attrs(FileAttrs::new(Size::Exact(size), ctx.stability));
            return Ok(ReadOutcome::Found(content.with_effects(effects)));
        }
    }
    Err(ProviderError::not_found(format!("field {name} not found")))
}

/// The per-mount facet axes: which template segments are identity-neutral
/// captures with finite choice sets, resolved to segment positions at mount
/// time. Mounting fails if a declared facet capture is missing from the
/// route template.
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

    /// Expand one concrete path into the cross product of all facet choices,
    /// substituting each choice into its capture's segment (prefix captures
    /// keep their prefix: a `v{version}` axis renders `v1`, `v2`). All
    /// expanded paths name the same logical object, so the host can answer
    /// any facet alias from one cached canonical.
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

fn representation_attrs(
    size: Size,
    stability: Stability,
    validator: Option<VersionToken>,
) -> FileAttrs {
    let attrs = FileAttrs::new(size, stability);
    if let Some(validator) = validator {
        attrs.with_version(validator)
    } else {
        attrs
    }
}

pub(super) fn body_file_content(
    bytes: Vec<u8>,
    ct: ContentType,
    stability: Stability,
    validator: Option<VersionToken>,
) -> FileContent {
    let size = Size::Exact(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    FileContent::new(bytes)
        .with_attrs(representation_attrs(size, stability, validator))
        .with_content_type(ct)
}
