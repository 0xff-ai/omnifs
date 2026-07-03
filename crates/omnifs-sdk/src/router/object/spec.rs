//! Object route specification and face registration.

use super::super::handlers::{RouteValidator, captures_validator};
use super::super::pattern::{Pattern, parse_pattern};
use super::dispatch::{
    BoxedFaceOpen, BoxedFaceRead, FaceHandler, LiveFaceKind, ObjectLeaf, ResolvedChildView,
};
use crate::captures::{Captures, FromCaptures};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::Stability;
use crate::handler::OpenedFile;
use crate::object::{FacetMetadata, Key, Load, Object, ObjectKind};
use crate::projection::FileProjection;
use crate::repr::{Format, RenderTable, Representable};
use omnifs_core::ContentType;
use std::future::Future;
use std::pin::Pin;

// ===========================================================================
// Handle + spec
// ===========================================================================

/// A detached object subtree, replayable at multiple alias templates.
///
/// Built by [`object()`]; mounted with
/// [`Router::object`](super::Router::object) or aliased with
/// [`Router::alias`](super::Router::alias). The spec is shared by `Rc`, so
/// aliasing the same handle twice replays one definition at two templates,
/// each with its own leaf claims.
pub struct ObjectHandle<O: Object> {
    pub(in crate::router) spec: std::rc::Rc<ObjectSpec<O>>,
}

/// Whether the anchor projects as a directory (children are the declared
/// faces) or as a single file (one canonical/representation/direct/blob face,
/// the path itself is the file).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::router) enum AnchorShape {
    Dir,
    File,
}

/// Internal registration state built by an object block, independent of any
/// alias template; [`mount_object`] specializes it per mount.
pub(in crate::router) struct ObjectSpec<O: Object> {
    pub(super) shape: AnchorShape,
    pub(super) when: Option<fn(&O::Key) -> bool>,
    pub(super) stability: fn(&O::Key) -> Stability,
    pub(crate) render_table: RenderTable,
    /// Whether a canonical face is declared (the render table source CT is
    /// always present, but an object may decline a canonical leaf when it has
    /// only direct/blob/stream faces).
    pub(super) has_canonical: bool,
    pub(super) leaves: Vec<ObjectLeaf<O>>,
    /// Boxed live-face handlers, shared so an alias replays the same closures.
    pub(super) face_handlers:
        std::rc::Rc<std::collections::BTreeMap<String, FaceHandler<O::State>>>,
    /// Collection faces declared on dir faces; resolved against the object
    /// registry at seal time.
    pub(super) collections: std::rc::Rc<Vec<CollectionDecl>>,
    /// SDK-generated collection list handlers, keyed by collection dir path,
    /// each with the late-bound child view that seal resolves and the dispatch
    /// path passes in.
    pub(super) collection_handlers: std::rc::Rc<Vec<CollectionHandlerEntry<O::State>>>,
    /// Tree faces declared on dir faces (`o.dir(name).tree(method)`): each is
    /// registered as a treeref route at `template/name` so a lookup/list there
    /// returns the subtree handoff. Shared so an alias replays the same closures.
    pub(super) tree_faces: std::rc::Rc<Vec<TreeFaceEntry<O::State>>>,
    /// Choices faces declared on dir faces (`o.dir(name).choices(names)`).
    pub(super) choices_faces: std::rc::Rc<Vec<ChoicesFace>>,
}

/// One tree face: the relative dir name and the boxed treeref handler. The
/// handler parses `O::Key` from the dir-path captures and runs the typed tree
/// method, lowering to a [`crate::handler::TreeRef`] (the host resolves the
/// git-open / archive callout the method issues).
pub(in crate::router) struct TreeFaceEntry<S> {
    pub name: &'static str,
    pub handler: super::super::handlers::BoxedTreeRefHandler<S>,
    pub validator: RouteValidator,
}

/// One choices face: the dir face name plus the fixed finite child names. The
/// mount registers an exhaustive dir route at `template/name` so a readdir
/// lists exactly these names as directories.
#[derive(Clone)]
pub(in crate::router) struct ChoicesFace {
    pub name: &'static str,
    pub names: &'static [&'static str],
}

/// One SDK-generated collection list handler: the collection dir path, the
/// boxed list handler, and the late-bound child view seal resolves.
pub(in crate::router) struct CollectionHandlerEntry<S> {
    pub dir_path: String,
    pub handler: CollectionHandler<S>,
    pub late_view: LateChildView,
    /// Capture validator for the deferred NESTED dir route, derived from the
    /// list method's key type `K` (the collection dir path captures), not the
    /// parent anchor's `O::Key`.
    pub validator: RouteValidator,
}

/// A boxed future yielding a [`crate::projection::DirProjection`].
pub(super) type DirProjectionFuture =
    Pin<Box<dyn Future<Output = Result<crate::projection::DirProjection>>>>;

/// The `dyn Fn` a [`CollectionHandler`] boxes: parse the parent key + cursor,
/// run the typed list method, lower the [`crate::collection::Collection`] to a
/// [`crate::projection::DirProjection`] against the seal-resolved child view.
type CollectionListFn<S> = dyn Fn(
    crate::handler::DirCx<S>,
    Captures,
    std::rc::Rc<ResolvedChildView>,
) -> DirProjectionFuture;

/// A boxed collection list handler, shared so an alias replays the same
/// closure.
pub(in crate::router) type CollectionHandler<S> = std::rc::Rc<CollectionListFn<S>>;

/// A late-bound child-view cell: the typed `collection` face stores the handler
/// at mount time, but the child object's template, leaves, and facet axes are
/// only known once every route is registered, so [`super::Router::seal`]
/// resolves them and fills this cell before dispatch can run.
pub(in crate::router) type LateChildView =
    std::rc::Rc<std::cell::RefCell<Option<std::rc::Rc<ResolvedChildView>>>>;

impl<O: Object> Clone for ObjectSpec<O> {
    fn clone(&self) -> Self {
        Self {
            shape: self.shape,
            when: self.when,
            stability: self.stability,
            render_table: self.render_table.clone(),
            has_canonical: self.has_canonical,
            leaves: self.leaves.clone(),
            face_handlers: self.face_handlers.clone(),
            collections: self.collections.clone(),
            collection_handlers: self.collection_handlers.clone(),
            tree_faces: self.tree_faces.clone(),
            choices_faces: self.choices_faces.clone(),
        }
    }
}

impl<O: Object> ObjectHandle<O> {
    /// Whether the object spec declares a canonical face (used by collection
    /// registry resolution).
    pub(in crate::router) fn has_canonical(&self) -> bool {
        self.spec.has_canonical
    }

    /// The collection faces this object declared, for registry resolution.
    pub(in crate::router) fn collection_decls(&self) -> &[CollectionDecl] {
        &self.spec.collections
    }

    /// The SDK-generated collection list handlers (dir path + boxed handler +
    /// late-bound child view).
    pub(in crate::router) fn collection_handlers(&self) -> &[CollectionHandlerEntry<O::State>] {
        &self.spec.collection_handlers
    }

    /// The tree faces declared on this object, registered as treeref routes at
    /// mount time.
    pub(in crate::router) fn tree_faces(&self) -> &[TreeFaceEntry<O::State>] {
        &self.spec.tree_faces
    }

    /// The choices faces declared on this object, registered as exhaustive dir
    /// routes at mount time.
    pub(in crate::router) fn choices_faces(&self) -> &[ChoicesFace] {
        &self.spec.choices_faces
    }

    /// The object spec's declared canonical-view leaf names (canonical,
    /// representation, derived), for collection child-view resolution.
    pub(in crate::router) fn canonical_view_leaf_names(&self) -> Vec<String> {
        self.spec
            .leaves
            .iter()
            .filter(|leaf| leaf.is_canonical_view())
            .map(|leaf| leaf.leaf_name().to_string())
            .collect()
    }
}

// ===========================================================================
// Faces
// ===========================================================================

/// Project a field leaf from a loaded object value and the route key: a pure
/// function, no callouts.
pub type DeriveFn<O> = fn(&O, &<O as Object>::Key) -> Result<FileProjection>;
// ===========================================================================
// Block builder
// ===========================================================================

/// The object block builder yielded to `r.object::<O>(template, |o| { .. })`.
/// Faces are declared with [`Self::file`] / [`Self::dir`]. A stability
/// declaration is mandatory; the block fails to finish otherwise.
pub struct ObjectBlock<O: Object> {
    template: &'static str,
    shape: AnchorShape,
    when: Option<fn(&O::Key) -> bool>,
    stability: Option<fn(&O::Key) -> Stability>,
    canonical_ct: Option<ContentType>,
    renders: Vec<(ContentType, crate::repr::RenderFn)>,
    leaves: Vec<ObjectLeaf<O>>,
    leaf_claims: Vec<Pattern>,
    /// Boxed live-face handlers (direct/blob/stream/object), keyed by leaf
    /// name; moved into the mounted entry.
    face_handlers: std::collections::BTreeMap<String, FaceHandler<O::State>>,
    /// Collections declared on dir faces, resolved against the object registry
    /// at seal time.
    collections: Vec<CollectionDecl>,
    /// SDK-generated collection list handlers (dir path + boxed handler +
    /// late-bound child view).
    collection_handlers: Vec<CollectionHandlerEntry<O::State>>,
    /// Tree faces (`o.dir(name).tree(method)`), registered as treeref routes at
    /// mount time.
    tree_faces: Vec<TreeFaceEntry<O::State>>,
    /// Choices faces (`o.dir(name).choices(names)`): the dir face name plus the
    /// fixed finite child names, registered as an exhaustive dir route at mount
    /// time so a readdir lists those names.
    choices_faces: Vec<ChoicesFace>,
    /// The single allowed file-object face name (file shape only).
    file_face_seen: bool,
    error: Option<ProviderError>,
}

/// A collection face declaration captured at registration time. The child
/// template + anchor computation are resolved against the object registry at
/// seal time, where every object route is known.
pub(in crate::router) struct CollectionDecl {
    /// The full dir path of the collection (`template/name`).
    pub dir_path: String,
    /// The parent object's template (`dir_path` minus the face name).
    pub parent_template: String,
    pub child_kind: ObjectKind,
    /// Whether any entry can be `fresh` (requires the child to have a canonical
    /// face). Always true for the typed `collection::<C>` form.
    pub requires_canonical: bool,
}

impl<O: Object> ObjectBlock<O> {
    fn new(template: &'static str, shape: AnchorShape) -> Result<Self> {
        parse_pattern(template)?;
        Ok(Self {
            template,
            shape,
            when: None,
            stability: None,
            canonical_ct: None,
            renders: Vec::new(),
            leaves: Vec::new(),
            leaf_claims: Vec::new(),
            face_handlers: std::collections::BTreeMap::new(),
            collections: Vec::new(),
            collection_handlers: Vec::new(),
            tree_faces: Vec::new(),
            choices_faces: Vec::new(),
            file_face_seen: false,
            error: None,
        })
    }

    /// Begin a file face under the anchor. `name` is a literal leaf (it may
    /// contain `/` for nested leaves like `actions/runs`).
    pub fn file(&mut self, name: &'static str) -> FileFace<'_, O> {
        FileFace {
            block: self,
            name,
            lazy: false,
        }
    }

    /// Begin a dir face under the anchor.
    pub fn dir(&mut self, name: &'static str) -> DirFace<'_, O> {
        DirFace { block: self, name }
    }

    /// The single anchor face of a file-shaped object: the anchor path IS the
    /// file. Only legal on `r.file_object`; on a dir-shaped anchor each face is
    /// named with `o.file(name).<face>()`.
    fn file_face_direct(&mut self) -> Result<FileFace<'_, O>> {
        if self.shape != AnchorShape::File {
            return Err(ProviderError::invalid_input(format!(
                "object route {}: a directly-declared face (o.canonical/representation/direct/blob) \
                 is only valid on a file-shaped object; on a dir object use o.file(name).<face>()",
                self.template
            )));
        }
        Ok(FileFace {
            block: self,
            name: "",
            lazy: false,
        })
    }

    /// Declare the file-object anchor's single canonical face: the anchor path
    /// serves the verbatim upstream bytes. File shape only.
    pub fn canonical<F: Format>(&mut self) -> Result<&mut Self> {
        self.file_face_direct()?.canonical::<F>()
    }

    /// Declare the file-object anchor as a rendered representation. File shape
    /// only.
    pub fn representation<F: Format>(&mut self) -> Result<&mut Self>
    where
        O: Representable<F>,
    {
        self.file_face_direct()?.representation::<F>()
    }

    /// Declare the file-object anchor as a direct face (invokes upstream on the
    /// read). File shape only.
    pub fn direct<Fut>(&mut self, method: fn(Cx<O::State>, O::Key) -> Fut) -> Result<&mut Self>
    where
        Fut: Future<Output = Result<FileProjection>> + 'static,
        O: 'static,
        O::State: 'static,
    {
        self.file_face_direct()?.direct(method)
    }

    /// Declare the file-object anchor as a blob face (host-resident bytes). File
    /// shape only.
    pub fn blob<F, Fut>(&mut self, method: fn(Cx<O::State>, O::Key) -> Fut) -> Result<&mut Self>
    where
        F: Format,
        Fut: Future<Output = Result<crate::projection::BlobFile<F>>> + 'static,
        O: 'static,
        O::State: 'static,
    {
        self.file_face_direct()?.blob(method)
    }

    /// Gate the whole object on a key predicate. A key that fails the predicate
    /// behaves as not-found for both listing and reads; no load is attempted.
    pub fn when(&mut self, pred: fn(&O::Key) -> bool) -> Result<&mut Self> {
        self.when = Some(pred);
        Ok(self)
    }

    /// Declare the object's [`Stability`] as a function of its key, shared by
    /// the canonical and every leaf derived from it (a rendering inherits the
    /// canonical's). Mandatory, once per block; the block fails to finish
    /// otherwise. For a stability that is the same for every key, prefer the
    /// [`Self::stable`] / [`Self::dynamic`] / [`Self::live`] shorthands.
    pub fn stability(&mut self, f: fn(&O::Key) -> Stability) -> &mut Self {
        self.stability = Some(f);
        self
    }

    /// Shorthand for `stability(|_| Stability::Stable)`.
    pub fn stable(&mut self) -> &mut Self {
        self.stability(|_| Stability::Stable)
    }

    /// Shorthand for `stability(|_| Stability::Dynamic)`.
    pub fn dynamic(&mut self) -> &mut Self {
        self.stability(|_| Stability::Dynamic)
    }

    /// Shorthand for `stability(|_| Stability::Live)`.
    pub fn live(&mut self) -> &mut Self {
        self.stability(|_| Stability::Live)
    }

    fn claim_leaf(&mut self, name: &str) -> Result<()> {
        // A file-shaped anchor IS its single face: the path itself is the leaf,
        // so there is no `template/name` child to claim (and `name` is empty).
        // The anchor pattern is claimed once at mount; claiming a synthetic
        // `template/` child here would record a bogus path.
        if self.shape == AnchorShape::File {
            return Ok(());
        }
        let pattern = parse_pattern(&format!("{}/{}", self.template.trim_end_matches('/'), name))?;
        self.leaf_claims.push(pattern);
        Ok(())
    }

    fn finish(mut self) -> Result<ObjectSpec<O>> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        let face_handlers = std::mem::take(&mut self.face_handlers);
        let collections = std::mem::take(&mut self.collections);
        let collection_handlers = std::mem::take(&mut self.collection_handlers);
        let tree_faces = std::mem::take(&mut self.tree_faces);
        let choices_faces = std::mem::take(&mut self.choices_faces);
        let stability = self.stability.ok_or_else(|| {
            ProviderError::invalid_input(
                "object block requires a stability declaration: stability(|key| ..) or stable()/dynamic()/live()",
            )
        })?;

        let has_canonical = self.canonical_ct.is_some();
        let source_ct = self.canonical_ct.unwrap_or(<O::Canonical as Format>::CT);
        let render_table = RenderTable::build(source_ct, self.renders)?;

        // A representation or derive face needs a canonical to render from.
        let has_render_or_derive = self.leaves.iter().any(|leaf| {
            matches!(
                leaf,
                ObjectLeaf::Representation { .. } | ObjectLeaf::Derived { .. }
            )
        });
        if has_render_or_derive && !has_canonical {
            return Err(ProviderError::invalid_input(format!(
                "object route {}: a representation/derive face requires a canonical face",
                self.template
            )));
        }

        Ok(ObjectSpec {
            shape: self.shape,
            when: self.when,
            stability,
            render_table,
            has_canonical,
            leaves: self.leaves,
            face_handlers: std::rc::Rc::new(face_handlers),
            collections: std::rc::Rc::new(collections),
            collection_handlers: std::rc::Rc::new(collection_handlers),
            tree_faces: std::rc::Rc::new(tree_faces),
            choices_faces: std::rc::Rc::new(choices_faces),
        })
    }
}

/// A pending file face named in [`ObjectBlock::file`]; finish with one of the
/// face methods (`canonical`/`representation`/`derive`/`object`/`direct`/
/// `blob`/`stream`).
pub struct FileFace<'a, O: Object> {
    block: &'a mut ObjectBlock<O>,
    name: &'static str,
    lazy: bool,
}

impl<'a, O: Object> FileFace<'a, O> {
    /// Exclude a derived leaf from listing-time eager preloads; reads still
    /// serve it from canonical bytes. Use for large fields (an issue body).
    #[must_use]
    pub fn lazy(mut self) -> Self {
        self.lazy = true;
        self
    }

    fn file_shape_guard(&mut self) -> Result<()> {
        if self.block.shape == AnchorShape::File {
            if self.block.file_face_seen {
                return Err(ProviderError::invalid_input(format!(
                    "file-object {} allows exactly one face",
                    self.block.template
                )));
            }
            self.block.file_face_seen = true;
        }
        Ok(())
    }

    /// The canonical source leaf: the verbatim upstream bytes. Exactly one
    /// canonical face per object; its `F` MUST equal [`Object::Canonical`].
    pub fn canonical<F: Format>(mut self) -> Result<&'a mut ObjectBlock<O>> {
        self.file_shape_guard()?;
        if F::CT != <O::Canonical as Format>::CT {
            return Err(ProviderError::invalid_input(format!(
                "object route {}: canonical face content type {:?} does not match Object::Canonical {:?}",
                self.block.template,
                F::CT,
                <O::Canonical as Format>::CT
            )));
        }
        if self.block.canonical_ct.is_some() {
            return Err(ProviderError::invalid_input(format!(
                "object route {}: more than one canonical face",
                self.block.template
            )));
        }
        self.block.canonical_ct = Some(F::CT);
        self.block.claim_leaf(self.name)?;
        // For the file shape the path itself is the leaf; record the literal.
        let leaf_name = self.leaf_name();
        self.block.leaves.push(ObjectLeaf::Canonical {
            leaf_name,
            ct: F::CT,
        });
        Ok(self.block)
    }

    /// A rendered representation of the canonical bytes via `Representable<F>`.
    pub fn representation<F: Format>(mut self) -> Result<&'a mut ObjectBlock<O>>
    where
        O: Representable<F>,
    {
        self.file_shape_guard()?;
        self.block.renders.push((F::CT, render_fn::<O, F>()));
        self.block.claim_leaf(self.name)?;
        let leaf_name = self.leaf_name();
        self.block.leaves.push(ObjectLeaf::Representation {
            leaf_name,
            ct: F::CT,
        });
        Ok(self.block)
    }

    /// A derived field leaf computed from the loaded object value. Eager by
    /// default (preloaded when the anchor or collection entry is listed; must
    /// be inline bytes); [`Self::lazy`] excludes it from preload.
    pub fn derive(mut self, method: DeriveFn<O>) -> Result<&'a mut ObjectBlock<O>> {
        self.file_shape_guard()?;
        self.block.claim_leaf(self.name)?;
        let leaf_name = self.leaf_name();
        let lazy = self.lazy;
        self.block.leaves.push(ObjectLeaf::Derived {
            leaf_name,
            derive: method,
            lazy,
        });
        Ok(self.block)
    }

    /// A direct face: invokes the provider/upstream on the read. The
    /// projection's source is `Body` (whole-read) or `Ranged`. NOT cached as
    /// canonical, NOT object-shaped.
    pub fn direct<Fut>(
        mut self,
        method: fn(Cx<O::State>, O::Key) -> Fut,
    ) -> Result<&'a mut ObjectBlock<O>>
    where
        Fut: Future<Output = Result<FileProjection>> + 'static,
        O: 'static,
        O::State: 'static,
    {
        self.file_shape_guard()?;
        self.block.claim_leaf(self.name)?;
        let leaf_name = self.leaf_name();
        let handler: BoxedFaceRead<O::State> = Box::new(move |cx, caps| {
            let cx = cx.clone();
            Box::pin(async move {
                let key = O::Key::from_captures(&caps)?;
                method(cx, key).await
            })
        });
        self.block
            .face_handlers
            .insert(leaf_name.clone(), FaceHandler::Direct(handler));
        self.block.leaves.push(ObjectLeaf::Live {
            leaf_name,
            kind: LiveFaceKind::Direct,
        });
        Ok(self.block)
    }

    /// A blob face: host-resident bytes fetched via `fetch-blob`; only a
    /// [`crate::blob::BlobId`] handle crosses back.
    pub fn blob<F, Fut>(
        mut self,
        method: fn(Cx<O::State>, O::Key) -> Fut,
    ) -> Result<&'a mut ObjectBlock<O>>
    where
        F: Format,
        Fut: Future<Output = Result<crate::projection::BlobFile<F>>> + 'static,
        O: 'static,
        O::State: 'static,
    {
        self.file_shape_guard()?;
        self.block.claim_leaf(self.name)?;
        let leaf_name = self.leaf_name();
        let handler: BoxedFaceRead<O::State> = Box::new(move |cx, caps| {
            let cx = cx.clone();
            Box::pin(async move {
                let key = O::Key::from_captures(&caps)?;
                Ok(method(cx, key).await?.into_projection())
            })
        });
        self.block
            .face_handlers
            .insert(leaf_name.clone(), FaceHandler::Direct(handler));
        self.block.leaves.push(ObjectLeaf::Live {
            leaf_name,
            kind: LiveFaceKind::Blob,
        });
        Ok(self.block)
    }

    /// A stream face: ranged or live bytes served through `open-file`. The
    /// ONLY face that may be `Live`.
    pub fn stream<R, Fut>(
        mut self,
        method: fn(Cx<O::State>, O::Key) -> Fut,
    ) -> Result<&'a mut ObjectBlock<O>>
    where
        R: Into<crate::projection::StreamFile>,
        Fut: Future<Output = Result<R>> + 'static,
        O: 'static,
        O::State: 'static,
    {
        self.file_shape_guard()?;
        self.block.claim_leaf(self.name)?;
        let leaf_name = self.leaf_name();
        let handler: BoxedFaceOpen<O::State> = Box::new(move |cx, caps| {
            let cx = cx.clone();
            Box::pin(async move {
                let key = O::Key::from_captures(&caps)?;
                let stream: crate::projection::StreamFile = method(cx, key).await?.into();
                Ok(OpenedFile::new(stream.attrs(), stream.reader()))
            })
        });
        self.block
            .face_handlers
            .insert(leaf_name.clone(), FaceHandler::Stream(handler));
        self.block.leaves.push(ObjectLeaf::Live {
            leaf_name,
            kind: LiveFaceKind::Stream,
        });
        Ok(self.block)
    }

    /// An object face: the leaf is backed by a CHILD object `C` with its own
    /// load/decode/canonical contract. The child's key is derived from the
    /// parent route captures.
    pub fn object<C>(mut self) -> Result<&'a mut ObjectBlock<O>>
    where
        C: Object<State = O::State> + 'static,
        C::Key: Key + FacetMetadata + 'static,
        O: 'static,
        O::State: 'static,
    {
        self.file_shape_guard()?;
        self.block.claim_leaf(self.name)?;
        let leaf_name = self.leaf_name();
        // The child object serves its own canonical bytes through a direct
        // read of its load result. The child key is parsed from the parent
        // captures (it must be `FromCaptures`-constructible from them).
        let handler: BoxedFaceRead<O::State> = Box::new(move |cx, caps| {
            let cx = cx.clone();
            Box::pin(async move {
                let key = C::Key::from_captures(&caps)?;
                let since = cx.version().cloned();
                match C::load(&cx, &key, since).await? {
                    Load::Fresh { canonical, .. } => Ok(FileProjection::body(canonical.bytes)
                        .content_type(<C::Canonical as Format>::CT)
                        .build()),
                    Load::Unchanged => Err(ProviderError::internal(
                        "object face returned Unchanged without a cached canonical",
                    )),
                    Load::NotFound => Err(ProviderError::not_found("child object not found")),
                }
            })
        });
        self.block
            .face_handlers
            .insert(leaf_name.clone(), FaceHandler::Direct(handler));
        self.block.leaves.push(ObjectLeaf::Live {
            leaf_name,
            kind: LiveFaceKind::Object,
        });
        Ok(self.block)
    }

    fn leaf_name(&self) -> String {
        match self.block.shape {
            // The file-object anchor's path IS the file; the leaf name is the
            // anchor's own last segment for dispatch.
            AnchorShape::File => self
                .block
                .template
                .rsplit('/')
                .next()
                .unwrap_or(self.name)
                .to_string(),
            AnchorShape::Dir => self.name.to_string(),
        }
    }
}

/// A pending dir face named in [`ObjectBlock::dir`].
pub struct DirFace<'a, O: Object> {
    block: &'a mut ObjectBlock<O>,
    name: &'static str,
}

impl<'a, O: Object> DirFace<'a, O> {
    /// Register a child-object collection: the dir lists `C` entries, each
    /// resolving to a child `r.object::<C>` anchor. `C` must be registered as
    /// its own object route (checked at seal time).
    ///
    /// The list `method`'s key `K` is parsed from the COLLECTION DIR PATH
    /// captures (`template/name`), not the parent anchor's `O::Key`. A
    /// collection under a captured sub-dir (`o.dir("issues/{filter}")`) can
    /// therefore read `{filter}` from `K`; for a collection whose dir path
    /// carries no extra captures, `K` is just `O::Key`. `C` is inferred from
    /// the method's `Collection<C, Cur>` return, so the call site needs no
    /// turbofish.
    pub fn collection<C, K, Fut, Cur>(
        self,
        method: fn(K, crate::collection::ListCx<Cur, O::State>) -> Fut,
    ) -> Result<&'a mut ObjectBlock<O>>
    where
        C: Object + 'static,
        C::Key: Key + 'static,
        K: FromCaptures + 'static,
        Cur: crate::collection::Cursor + 'static,
        Fut: Future<Output = Result<crate::collection::Collection<C, Cur>>> + 'static,
        O: 'static,
        O::State: 'static,
    {
        if self.name.starts_with('@') {
            return Err(ProviderError::invalid_input(format!(
                "object route {}: collection child {:?} uses the reserved @ namespace",
                self.block.template, self.name
            )));
        }
        let dir_path = format!(
            "{}/{}",
            self.block.template.trim_end_matches('/'),
            self.name
        );
        let late_view: LateChildView = std::rc::Rc::new(std::cell::RefCell::new(None));
        self.block.collections.push(CollectionDecl {
            dir_path: dir_path.clone(),
            parent_template: self.block.template.to_string(),
            child_kind: C::kind(),
            requires_canonical: true,
        });

        // The SDK-generated collection list handler: parse the parent key + the
        // host-echoed cursor, run the typed list method, and lower the
        // Collection to a DirProjection against the seal-resolved child view.
        let handler: CollectionHandler<O::State> = std::rc::Rc::new(
            move |dir_cx: crate::handler::DirCx<O::State>,
                  caps: Captures,
                  child_view: std::rc::Rc<ResolvedChildView>| {
                Box::pin(async move {
                    let key = K::from_captures(&caps)?;
                    let cursor = match dir_cx.cursor() {
                        Some(wire) => crate::collection::decode_cursor::<Cur>(wire)?,
                        None => None,
                    };
                    let cx = (*dir_cx).clone();
                    let list_cx = crate::collection::ListCx::new(cx, cursor);
                    let collection = method(key, list_cx).await?;
                    crate::collection::collection_to_dir_projection::<C, Cur>(
                        &child_view,
                        collection,
                    )
                }) as DirProjectionFuture
            },
        );
        self.block.collection_handlers.push(CollectionHandlerEntry {
            dir_path,
            handler,
            late_view,
            validator: captures_validator::<K>(),
        });
        Ok(self.block)
    }

    /// A subtree handoff registered as an object dir face so it reads in
    /// `start()`. Lowers to the `treeref` machinery: the face becomes a treeref
    /// route at `template/name`, so a lookup or list there returns the subtree
    /// handoff after the host runs the git-open / archive callout the method
    /// issues.
    pub fn tree<Fut>(
        self,
        method: fn(Cx<O::State>, O::Key) -> Fut,
    ) -> Result<&'a mut ObjectBlock<O>>
    where
        Fut: Future<Output = Result<crate::handler::TreeRef>> + 'static,
        O: 'static,
        O::State: 'static,
    {
        if self.name.starts_with('@') {
            return Err(ProviderError::invalid_input(format!(
                "object route {}: tree child {:?} uses the reserved @ namespace",
                self.block.template, self.name
            )));
        }
        // The treeref route (registered at mount time) claims the path, so the
        // tree face must NOT also claim_leaf or the seal overlap check fires.
        let handler: super::super::handlers::BoxedTreeRefHandler<O::State> =
            std::sync::Arc::new(move |cx: Cx<O::State>, caps: Captures| {
                Box::pin(async move {
                    let key = O::Key::from_captures(&caps)?;
                    method(cx, key).await
                }) as Pin<Box<dyn Future<Output = Result<crate::handler::TreeRef>>>>
            });
        self.block.tree_faces.push(TreeFaceEntry {
            name: self.name,
            handler,
            validator: captures_validator::<O::Key>(),
        });
        Ok(self.block)
    }

    /// Declare an exhaustive listing of a fixed finite name set: each name is a
    /// child directory (the `StateFilter::choices()` axis). The mount registers
    /// an exhaustive dir route at `template/name` so a readdir lists exactly
    /// these names.
    pub fn choices(self, names: &'static [&'static str]) -> Result<&'a mut ObjectBlock<O>> {
        for name in names {
            if name.starts_with('@') {
                return Err(ProviderError::invalid_input(format!(
                    "object route {}: choices child {name:?} uses the reserved @ namespace",
                    self.block.template
                )));
            }
        }
        // The dir route (registered at mount time) claims the path, so the
        // choices face must NOT also claim_leaf.
        self.block.choices_faces.push(ChoicesFace {
            name: self.name,
            names,
        });
        Ok(self.block)
    }
}

// ===========================================================================
// Object definition + mount
// ===========================================================================

/// Define a detached dir-shaped object subtree, mounted later with
/// [`Router::object`](super::Router::object) /
/// [`Router::alias`](super::Router::alias). `template` must be absolute.
pub fn object<O: Object>(
    template: &'static str,
    block: impl FnOnce(&mut ObjectBlock<O>) -> Result<()>,
) -> Result<ObjectHandle<O>> {
    let mut builder = ObjectBlock::new(template, AnchorShape::Dir)?;
    block(&mut builder)?;
    let spec = builder.finish()?;
    Ok(ObjectHandle {
        spec: std::rc::Rc::new(spec),
    })
}

/// Define a file-shaped object anchor: the path projects as a single file
/// (one canonical/representation/direct/blob face), not a directory.
pub(in crate::router) fn file_object<O: Object>(
    template: &'static str,
    block: impl FnOnce(&mut ObjectBlock<O>) -> Result<()>,
) -> Result<ObjectHandle<O>> {
    let mut builder = ObjectBlock::new(template, AnchorShape::File)?;
    block(&mut builder)?;
    if !builder.file_face_seen {
        return Err(ProviderError::invalid_input(format!(
            "file-object {template} requires exactly one canonical/representation/direct/blob face"
        )));
    }
    let spec = builder.finish()?;
    Ok(ObjectHandle {
        spec: std::rc::Rc::new(spec),
    })
}

fn render_fn<O, F>() -> crate::repr::RenderFn
where
    O: Object + Representable<F>,
    F: Format,
{
    |canonical| O::decode(canonical).map(|value| value.represent())
}
