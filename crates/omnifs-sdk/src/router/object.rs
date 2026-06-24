//! Object route registration, read/list path, and view-leaf expansion.
//!
//! An object route binds a typed [`Object`] to a template: the key parsed
//! from the captures both identifies the resource ([`Key::anchor`]) and
//! loads it ([`Object::load`]). The block passed to `r.object::<O>(..)`
//! declares the anchor's faces ([`ObjectBlock`]):
//!
//! - [`FileFace`] (`o.file(name)`): a `canonical`, `representation`,
//!   `derive`, `object`, `direct`, `blob`, or `stream` leaf.
//! - [`DirFace`] (`o.dir(name)`): a `collection`, `choices`, `children`, or
//!   `tree` child topology.
//!
//! The cache contract (the host owns all caching; the SDK only emits
//! effects): a fresh [`Object::load`] produces a `canonical-store` effect
//! carrying the verbatim upstream bytes, the validator, and the expanded
//! view-leaf paths that map back to the object's logical id. On a later read
//! the host pushes those bytes back as a [`CachedCanonical`] and the SDK
//! re-renders without an upstream call. Facets (identity-neutral captures
//! with finite choices) multiply the view leaves, so loading
//! `/issues/open/7/title.txt` also teaches the host
//! `/issues/all/7/title.txt`.

use super::pattern::{CaptureLocation, Pattern};
use crate::browse::{CachedCanonical, Effects, FileContent, ReadOutcome};
use crate::captures::{Captures, FromCaptures};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::{FileAttrs, FileProj, ProjBytes, ReadMode, Size, Stability, VersionToken};
use crate::handler::OpenedFile;
use crate::object::{FacetAxis, FacetMetadata, Key, Load, Object, ObjectKind};
use crate::projection::FileProjection;
use crate::repr::{Format, RenderTable, Representable};
use omnifs_core::ContentType;
use std::future::Future;
use std::pin::Pin;

use super::handlers::{RouteValidator, captures_validator};
use super::pattern::parse_pattern;

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
    pub(super) spec: std::rc::Rc<ObjectSpec<O>>,
}

/// Whether the anchor projects as a directory (children are the declared
/// faces) or as a single file (one canonical/representation/direct/blob face,
/// the path itself is the file).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AnchorShape {
    Dir,
    File,
}

/// Internal registration state built by an object block, independent of any
/// alias template; [`mount_object`] specializes it per mount.
pub(super) struct ObjectSpec<O: Object> {
    pub shape: AnchorShape,
    pub when: Option<fn(&O::Key) -> bool>,
    pub stability: fn(&O::Key) -> Stability,
    pub render_table: RenderTable,
    /// Whether a canonical face is declared (the render table source CT is
    /// always present, but an object may decline a canonical leaf when it has
    /// only direct/blob/stream faces).
    pub has_canonical: bool,
    pub leaves: Vec<ObjectLeaf<O>>,
    /// Boxed live-face handlers, shared so an alias replays the same closures.
    pub face_handlers: std::rc::Rc<std::collections::BTreeMap<String, FaceHandler<O::State>>>,
    /// Collection faces declared on dir faces; resolved against the object
    /// registry at seal time.
    pub collections: std::rc::Rc<Vec<CollectionDecl>>,
    /// SDK-generated collection list handlers, keyed by collection dir path,
    /// each with the late-bound child view that seal resolves and the dispatch
    /// path passes in.
    pub collection_handlers: std::rc::Rc<Vec<CollectionHandlerEntry<O::State>>>,
    /// Tree faces declared on dir faces (`o.dir(name).tree(method)`): each is
    /// registered as a treeref route at `template/name` so a lookup/list there
    /// returns the subtree handoff. Shared so an alias replays the same closures.
    pub tree_faces: std::rc::Rc<Vec<TreeFaceEntry<O::State>>>,
    /// Choices faces declared on dir faces (`o.dir(name).choices(names)`).
    pub choices_faces: std::rc::Rc<Vec<ChoicesFace>>,
}

/// One tree face: the relative dir name and the boxed treeref handler. The
/// handler parses `O::Key` from the dir-path captures and runs the typed tree
/// method, lowering to a [`crate::handler::TreeRef`] (the host resolves the
/// git-open / archive callout the method issues).
pub(super) struct TreeFaceEntry<S> {
    pub name: &'static str,
    pub handler: super::handlers::BoxedTreeRefHandler<S>,
    pub validator: RouteValidator,
}

/// One choices face: the dir face name plus the fixed finite child names. The
/// mount registers an exhaustive dir route at `template/name` so a readdir
/// lists exactly these names as directories.
#[derive(Clone)]
pub(super) struct ChoicesFace {
    pub name: &'static str,
    pub names: &'static [&'static str],
}

/// One SDK-generated collection list handler: the collection dir path, the
/// boxed list handler, and the late-bound child view seal resolves.
pub(super) struct CollectionHandlerEntry<S> {
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
pub(super) type CollectionHandler<S> = std::rc::Rc<CollectionListFn<S>>;

/// A late-bound child-view cell: the typed `collection` face stores the handler
/// at mount time, but the child object's template, leaves, and facet axes are
/// only known once every route is registered, so [`super::Router::seal`]
/// resolves them and fills this cell before dispatch can run.
pub(super) type LateChildView =
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
    pub(super) fn has_canonical(&self) -> bool {
        self.spec.has_canonical
    }

    /// The collection faces this object declared, for registry resolution.
    pub(super) fn collection_decls(&self) -> &[CollectionDecl] {
        &self.spec.collections
    }

    /// The SDK-generated collection list handlers (dir path + boxed handler +
    /// late-bound child view).
    pub(super) fn collection_handlers(&self) -> &[CollectionHandlerEntry<O::State>] {
        &self.spec.collection_handlers
    }

    /// The tree faces declared on this object, registered as treeref routes at
    /// mount time.
    pub(super) fn tree_faces(&self) -> &[TreeFaceEntry<O::State>] {
        &self.spec.tree_faces
    }

    /// The choices faces declared on this object, registered as exhaustive dir
    /// routes at mount time.
    pub(super) fn choices_faces(&self) -> &[ChoicesFace] {
        &self.spec.choices_faces
    }

    /// The object spec's declared canonical-view leaf names (canonical,
    /// representation, derived), for collection child-view resolution.
    pub(super) fn canonical_view_leaf_names(&self) -> Vec<String> {
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

/// Which kind of live face a [`ObjectLeaf::Live`] is: the dispatch path
/// differs (`Direct`/`Blob` serve through `read_file`, `Stream` through
/// `open_file`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LiveFaceKind {
    Direct,
    Blob,
    Stream,
    /// A child object served as a file (its own canonical contract).
    Object,
}

/// One declared face of an object anchor.
///
/// `Canonical`/`Representation`/`Derived` serve from the object's canonical
/// bytes (verbatim, rendered, or projected) and contribute view leaves to the
/// canonical-store effect. `Live` faces (direct/blob/stream/object) invoke a
/// boxed handler stored on the mounted entry and are NOT view leaves of the
/// parent canonical.
pub(super) enum ObjectLeaf<O: Object> {
    /// The verbatim canonical bytes themselves (`stem.ext`).
    Canonical { leaf_name: String, ct: ContentType },
    /// A registered render of the canonical bytes.
    Representation { leaf_name: String, ct: ContentType },
    /// A field leaf computed from the parsed object value. `lazy` excludes it
    /// from listing-time eager preloads; reads still serve it.
    Derived {
        leaf_name: String,
        derive: DeriveFn<O>,
        lazy: bool,
    },
    /// A live face served by a boxed handler keyed by `leaf_name` on the
    /// mounted entry.
    Live {
        leaf_name: String,
        kind: LiveFaceKind,
    },
}

impl<O: Object> Clone for ObjectLeaf<O> {
    fn clone(&self) -> Self {
        match self {
            Self::Canonical { leaf_name, ct } => Self::Canonical {
                leaf_name: leaf_name.clone(),
                ct: *ct,
            },
            Self::Representation { leaf_name, ct } => Self::Representation {
                leaf_name: leaf_name.clone(),
                ct: *ct,
            },
            Self::Derived {
                leaf_name,
                derive,
                lazy,
            } => Self::Derived {
                leaf_name: leaf_name.clone(),
                derive: *derive,
                lazy: *lazy,
            },
            Self::Live { leaf_name, kind } => Self::Live {
                leaf_name: leaf_name.clone(),
                kind: *kind,
            },
        }
    }
}

impl<O: Object> ObjectLeaf<O> {
    fn leaf_name(&self) -> &str {
        match self {
            Self::Canonical { leaf_name, .. }
            | Self::Representation { leaf_name, .. }
            | Self::Derived { leaf_name, .. }
            | Self::Live { leaf_name, .. } => leaf_name,
        }
    }

    /// Whether this leaf is a view of the canonical bytes (canonical,
    /// representation, derived) versus an independently served face.
    fn is_canonical_view(&self) -> bool {
        matches!(
            self,
            Self::Canonical { .. } | Self::Representation { .. } | Self::Derived { .. }
        )
    }

    /// The mount-time leaf kind for exact-name dispatch resolution.
    fn leaf_kind(&self) -> LeafKind {
        match self {
            Self::Canonical { .. } => LeafKind::Canonical,
            Self::Representation { ct, .. } => LeafKind::Representation(*ct),
            Self::Derived { .. } => LeafKind::Derived,
            Self::Live { kind, .. } => match kind {
                // Blob faces serve through the same boxed direct handler as a
                // direct face (the blob lowers to a `FileProjection::blob`).
                LiveFaceKind::Direct | LiveFaceKind::Blob => LeafKind::Direct,
                LiveFaceKind::Stream => LeafKind::Stream,
                LiveFaceKind::Object => LeafKind::Object,
            },
        }
    }
}

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
pub(super) struct CollectionDecl {
    /// The full dir path of the collection (`template/name`).
    pub dir_path: String,
    /// The parent object's template (`dir_path` minus the face name).
    pub parent_template: String,
    pub child_kind_str: &'static str,
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
            child_kind_str: C::kind().as_str(),
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
        let handler: super::handlers::BoxedTreeRefHandler<O::State> =
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
pub(super) fn file_object<O: Object>(
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

/// The mounted, type-erased object route the dispatch tables hold.
pub(super) struct ObjectRouteEntry<S> {
    pub pattern: Pattern,
    pub shape: AnchorShape,
    pub leaves: Vec<ListingLeaf>,
    pub read: BoxedObjectRead<S>,
    pub list: BoxedObjectList<S>,
    /// Per-leaf live-face handlers (direct/blob/stream/object), keyed by leaf
    /// name. Shared with the spec so an alias mount replays the same closures.
    pub face_handlers: std::rc::Rc<std::collections::BTreeMap<String, FaceHandler<S>>>,
    /// ANCHOR-topology collections attached at seal: their child template
    /// equals this anchor, so they merge into this anchor's listing/lookup
    /// instead of getting a separate dir route.
    pub anchor_collections: Vec<AnchorCollection<S>>,
    pub validator: RouteValidator,
}

/// An ANCHOR-topology collection attached to a parent object's anchor: the
/// boxed list handler plus the child view resolved at seal time. The parent's
/// anchor listing runs each of these, merges the child-name entries, and emits
/// each fresh child's canonical store.
pub(super) struct AnchorCollection<S> {
    pub handler: CollectionHandler<S>,
    pub child_view: std::rc::Rc<ResolvedChildView>,
}

impl<S> ObjectRouteEntry<S> {
    /// Run the attached ANCHOR collections and lower each to a
    /// [`crate::projection::DirProjection`], using the captures decoded from
    /// the anchor path.
    pub(super) async fn run_anchor_collections(
        &self,
        cx: &Cx<S>,
        caps: &Captures,
    ) -> Result<Vec<crate::projection::DirProjection>> {
        let mut projections = Vec::with_capacity(self.anchor_collections.len());
        for collection in &self.anchor_collections {
            let dir_cx = crate::handler::DirCx::new(
                cx.clone(),
                crate::handler::DirIntent::List { cursor: None },
            );
            let projection =
                (collection.handler)(dir_cx, caps.clone(), collection.child_view.clone()).await?;
            projections.push(projection);
        }
        Ok(projections)
    }

    /// Whether this anchor has ANCHOR-topology collections attached.
    pub(super) fn has_anchor_collections(&self) -> bool {
        !self.anchor_collections.is_empty()
    }
}

/// A live-face handler erased over its concrete future, plus how to dispatch
/// it (read-file vs open-file).
pub(super) enum FaceHandler<S> {
    /// A direct/blob/object face: served through `read_file`.
    Direct(BoxedFaceRead<S>),
    /// A stream face: served through `open_file`.
    Stream(BoxedFaceOpen<S>),
}

type BoxedFaceRead<S> = Box<
    dyn for<'a> Fn(
        &'a Cx<S>,
        Captures,
    ) -> Pin<Box<dyn Future<Output = Result<FileProjection>> + 'a>>,
>;
type BoxedFaceOpen<S> = Box<
    dyn for<'a> Fn(&'a Cx<S>, Captures) -> Pin<Box<dyn Future<Output = Result<OpenedFile>> + 'a>>,
>;

/// What kind of face a listed leaf is, resolved by exact leaf-name match (not
/// by extension): this is how a derive leaf named `notes.md` routes to its
/// derive fn rather than the Markdown representation render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LeafKind {
    Canonical,
    Representation(ContentType),
    Derived,
    Direct,
    Stream,
    Object,
}

/// A child name an object anchor lists, precomputed at mount time.
pub(super) struct ListingLeaf {
    pub name: String,
    /// The face kind this leaf resolves to (exact-name match drives dispatch).
    pub kind: LeafKind,
}

impl ListingLeaf {
    /// Whether the canonical-source leaf is stamped with the loaded byte length.
    pub(super) fn is_canonical(&self) -> bool {
        matches!(self.kind, LeafKind::Canonical)
    }
}

/// The typed runtime side of a mounted object.
struct ObjectRoute<O: Object> {
    pattern: Pattern,
    shape: AnchorShape,
    leaves: Vec<ObjectLeaf<O>>,
    stability: fn(&O::Key) -> Stability,
    render_table: RenderTable,
    has_canonical: bool,
    facet_expansion: FacetExpansion,
    when: Option<fn(&O::Key) -> bool>,
}

impl<O: Object> Clone for ObjectRoute<O> {
    fn clone(&self) -> Self {
        Self {
            pattern: self.pattern.clone(),
            shape: self.shape,
            leaves: self.leaves.clone(),
            stability: self.stability,
            render_table: self.render_table.clone(),
            has_canonical: self.has_canonical,
            facet_expansion: self.facet_expansion.clone(),
            when: self.when,
        }
    }
}

impl<O: Object + 'static> ObjectRoute<O>
where
    O::Key: Key + FacetMetadata + 'static,
{
    fn for_mount(spec: &ObjectSpec<O>, pattern: &Pattern) -> Result<Self> {
        Ok(Self {
            pattern: pattern.clone(),
            shape: spec.shape,
            leaves: spec.leaves.clone(),
            stability: spec.stability,
            render_table: spec.render_table.clone(),
            has_canonical: spec.has_canonical,
            facet_expansion: FacetExpansion::for_pattern::<O::Key>(pattern)?,
            when: spec.when,
        })
    }

    fn read_handler(self) -> BoxedObjectRead<O::State>
    where
        O::State: 'static,
    {
        Box::new(
            move |cx: &Cx<O::State>,
                  caps: Captures,
                  target: ObjectReadTarget,
                  cached: Option<CachedCanonical>,
                  read_path: String| {
                let route = self.clone();
                Box::pin(async move { route.read(cx, caps, target, cached, read_path).await })
            },
        )
    }

    fn list_handler(self) -> BoxedObjectList<O::State>
    where
        O::State: 'static,
    {
        Box::new(
            move |cx: &Cx<O::State>, caps: Captures, list_path: String| {
                let route = self.clone();
                Box::pin(async move { route.list(cx, caps, list_path).await })
            },
        )
    }

    /// The anchor-listing side effects: load the object and emit the
    /// canonical-store effect plus eager derived preloads.
    async fn list(
        &self,
        cx: &Cx<O::State>,
        caps: Captures,
        list_path: String,
    ) -> Result<ObjectListing> {
        let key = O::Key::from_captures(&caps)?;
        if self.when.is_some_and(|pred| !pred(&key)) {
            return Err(ProviderError::not_found(format!(
                "object not found: {list_path}"
            )));
        }
        if !self.has_canonical {
            // No canonical to store; the listing is purely the static leaf set.
            return Ok(ObjectListing {
                effects: Effects::new(),
                source: None,
            });
        }
        let stability = (self.stability)(&key);

        let since = cx.version().cloned();
        let (value, canonical, preloads) = match O::load(cx, &key, since).await? {
            Load::Fresh {
                value,
                canonical,
                preloads,
            } => (value, canonical, preloads),
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
        let source = SourceLeafAttrs {
            len: canonical.bytes.len() as u64,
            validator: canonical.validator.clone(),
            stability,
        };
        let id = key.anchor(O::kind());
        let mut effects = Effects::new();
        effects.canonical_store(
            &id,
            canonical.validator.clone(),
            canonical.bytes,
            self.view_leaves_for_base(&list_path)?,
        );
        self.project_eager_fields(&mut effects, &id, &value, &key, &list_path, stability)?;
        self.lower_preloads(&mut effects, preloads, &list_path, stability)?;
        Ok(ObjectListing {
            effects,
            source: Some(source),
        })
    }

    /// The object read path (warm, fresh, unchanged, not-found).
    async fn read(
        &self,
        cx: &Cx<O::State>,
        caps: Captures,
        target: ObjectReadTarget,
        cached: Option<CachedCanonical>,
        read_path: String,
    ) -> Result<ReadOutcome> {
        let key = O::Key::from_captures(&caps)?;
        if self.when.is_some_and(|pred| !pred(&key)) {
            return Ok(ReadOutcome::NotFound(None));
        }
        let stability = (self.stability)(&key);

        if let Some(ref push) = cached
            && push.matches_anchor(&key.anchor(O::kind()))
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
        let (value, canonical, preloads) = match O::load(cx, &key, since).await? {
            Load::Fresh {
                value,
                canonical,
                preloads,
            } => (value, canonical, preloads),
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
            Load::NotFound => return Ok(ReadOutcome::NotFound(Some(key.anchor(O::kind())))),
        };
        let id = key.anchor(O::kind());
        // The anchor base of the requested object is the path its canonical is
        // anchored at. For a dir-shaped anchor the read path is a leaf under
        // the anchor, so strip the leaf; for a file-shaped anchor the read
        // path IS the anchor (no leaf to strip). Both the stored view leaves
        // and the preloads' sibling paths are computed relative to this base.
        let anchor_base = match self.shape {
            AnchorShape::File => read_path.clone(),
            AnchorShape::Dir => read_path
                .rsplit_once('/')
                .map_or_else(|| read_path.clone(), |(base, _)| base.to_string()),
        };
        let mut effects = Effects::new();
        effects.canonical_store(
            &id,
            canonical.validator.clone(),
            canonical.bytes.clone(),
            // Store every canonical-view leaf, not just the requested one, so a
            // later warm read of a sibling representation hits the view cache
            // (consistent with the anchor-listing path).
            self.view_leaves_for_base(&anchor_base)?,
        );
        // For a file-shaped anchor, the just-read file must also appear in its
        // parent directory's listing (the day you read shows up in `ls`),
        // symmetric with how preloaded siblings are projected.
        if self.shape == AnchorShape::File {
            let mut file = FileProj::deferred(
                Size::Exact(canonical.bytes.len() as u64),
                ReadMode::Full,
                stability,
            );
            if let Some(v) = &canonical.validator {
                file = file.with_version(v.clone());
            }
            effects.project_file_with_id(&anchor_base, Some(&id), file)?;
            if let Some((parent, _)) = anchor_base.rsplit_once('/')
                && !parent.is_empty()
            {
                effects.project_dir(parent)?;
            }
        }
        self.lower_preloads(&mut effects, preloads, &anchor_base, stability)?;
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

    /// Every full path that maps to this object's canonical bytes, with the
    /// anchor at `base`: each canonical-view leaf under the anchor, multiplied
    /// across facet choices. For a file-shaped anchor `base` IS the single
    /// canonical-view file (there is no leaf to append).
    fn view_leaves_for_base(&self, base: &str) -> Result<Vec<String>> {
        if self.shape == AnchorShape::File {
            return self.facet_expansion.expand_view_leaves(base);
        }
        let mut view_leaves = Vec::new();
        for leaf in &self.leaves {
            if !leaf.is_canonical_view() {
                continue;
            }
            let leaf_path = format!("{base}/{}", leaf.leaf_name());
            view_leaves.extend(self.facet_expansion.expand_view_leaves(&leaf_path)?);
        }
        Ok(view_leaves)
    }

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
            let ObjectLeaf::Derived {
                leaf_name,
                derive,
                lazy,
            } = leaf
            else {
                continue;
            };
            if *lazy {
                continue;
            }
            let projection = derive(value, key)?;
            let Some(mut file) = projection
                .as_file_proj()
                .filter(|f| matches!(f.bytes, ProjBytes::Inline(_)))
            else {
                return Err(ProviderError::internal(format!(
                    "derived object leaf {leaf_name:?} cannot preload non-inline bytes"
                )));
            };
            file.attrs = FileAttrs::new(file.attrs.size, stability);
            effects.project_file_with_id(format!("{list_path}/{leaf_name}"), Some(id), file)?;
        }
        Ok(())
    }

    /// Lower the typed [`crate::object::Preloads`] from a fresh load onto the
    /// effects channel (R5):
    ///
    /// - `objects` (same-type siblings): store the sibling canonical against
    ///   its own anchor id, with view leaves computed from THIS object's
    ///   canonical-view faces (and facets) at the sibling's path. The sibling
    ///   path is `anchor_base` with each identity capture substituted to the
    ///   sibling's value.
    /// - `files`: `project_file`, accepting only inline/deferred sources
    ///   (`Body`/`Ranged`/`Blob` are a build error, "serve through its own
    ///   face").
    fn lower_preloads(
        &self,
        effects: &mut Effects,
        preloads: crate::object::Preloads,
        anchor_base: &str,
        stability: Stability,
    ) -> Result<()> {
        let (objects, files) = preloads.into_parts();

        // A preloaded sibling resolves on lookup, but it must also appear in its
        // parent directory's listing so `ls` of the fetched range shows it (the
        // SDK derives the directory effect from the preload path; spec Part 5).
        let mut parent_dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

        for sibling in objects {
            let sibling_base =
                self.substitute_identity_captures(anchor_base, &sibling.identity_captures);
            let validator = sibling.canonical.validator.clone();
            let bytes_len = sibling.canonical.bytes.len() as u64;
            let id = crate::identity::LogicalId::new(O::kind(), sibling.identity_captures);
            let view_leaves = self.view_leaves_for_base(&sibling_base)?;
            match self.shape {
                AnchorShape::File => {
                    // A file-shaped sibling projects as a deferred dirent with an
                    // honest size, listed in its parent dir; reads serve from the
                    // stored canonical.
                    let mut file =
                        FileProj::deferred(Size::Exact(bytes_len), ReadMode::Full, stability);
                    if let Some(v) = &validator {
                        file = file.with_version(v.clone());
                    }
                    effects.project_file_with_id(&sibling_base, Some(&id), file)?;
                    if let Some((parent, _)) = sibling_base.rsplit_once('/')
                        && !parent.is_empty()
                    {
                        parent_dirs.insert(parent.to_string());
                    }
                },
                AnchorShape::Dir => {
                    parent_dirs.insert(sibling_base.clone());
                },
            }
            effects.canonical_store(&id, validator, sibling.canonical.bytes, view_leaves);
        }

        for (path, file) in files {
            let proj = file.as_file_proj().ok_or_else(|| {
                ProviderError::invalid_input(format!(
                    "preload file {path:?} has a Body/Ranged/Blob source; serve it through its own face"
                ))
            })?;
            if let Some((parent, _)) = path.rsplit_once('/')
                && !parent.is_empty()
            {
                parent_dirs.insert(parent.to_string());
            }
            effects.project_file(&path, proj)?;
        }

        for dir in parent_dirs {
            effects.project_dir(&dir)?;
        }
        Ok(())
    }

    /// Substitute each identity capture into the anchor path at that capture's
    /// segment location, yielding the sibling object's anchor base path.
    fn substitute_identity_captures(
        &self,
        anchor_base: &str,
        captures: &[(&'static str, String)],
    ) -> String {
        let offset = usize::from(anchor_base.starts_with('/'));
        let mut segments = anchor_base
            .split('/')
            .map(str::to_string)
            .collect::<Vec<_>>();
        for (name, value) in captures {
            let Some(location) = self.pattern.capture_location(name) else {
                continue;
            };
            let idx = location.segment_index() + offset;
            if let Some(segment) = segments.get_mut(idx) {
                *segment = location.render_segment(value);
            }
        }
        segments.join("/")
    }
}

/// What mounting an object spec yields: the dispatchable entry and the leaf
/// claims to feed [`Router::seal`](super::Router::seal).
pub(super) struct MountedObject<S> {
    pub entry: ObjectRouteEntry<S>,
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

/// What an anchor's list dispatch needs.
pub(super) struct ObjectListing {
    pub effects: Effects,
    pub source: Option<SourceLeafAttrs>,
}

pub(super) struct SourceLeafAttrs {
    pub len: u64,
    pub validator: Option<VersionToken>,
    pub stability: Stability,
}

/// Which child of the anchor a read addresses.
pub(super) enum ObjectReadTarget {
    /// The verbatim canonical bytes.
    Canonical,
    /// A rendered representation by content type.
    Representation(ContentType),
    /// A derived field by leaf name.
    Derived(String),
    /// A direct face by leaf name (served through `read_file`).
    Direct(String),
    /// A stream face by leaf name (served through `open_file`).
    Stream(String),
}

fn mounted_leaf_claims<O: Object>(
    spec: &ObjectSpec<O>,
    mount_template: &str,
) -> Result<Vec<Pattern>> {
    let mount = mount_template.trim_end_matches('/');
    let mut claims = Vec::new();
    if spec.shape == AnchorShape::File {
        // The file-object anchor IS the leaf; no separate child claims.
        return Ok(claims);
    }
    for leaf in &spec.leaves {
        claims.push(parse_pattern(&format!("{mount}/{}", leaf.leaf_name()))?);
    }
    Ok(claims)
}

/// Specialize an [`ObjectSpec`] at a concrete mount pattern.
pub(super) fn mount_object<O>(
    pattern: &Pattern,
    spec: &ObjectSpec<O>,
    combined_template: &str,
) -> Result<MountedObject<O::State>>
where
    O: Object + 'static,
    O::Key: Key + FacetMetadata + 'static,
    O::State: 'static,
{
    let listing_leaves: Vec<ListingLeaf> = spec
        .leaves
        .iter()
        .map(|leaf| ListingLeaf {
            name: leaf.leaf_name().to_string(),
            kind: leaf.leaf_kind(),
        })
        .collect();

    let mut leaf_claims = mounted_leaf_claims(spec, combined_template)?;
    leaf_claims.push(pattern.clone());

    let route = ObjectRoute::for_mount(spec, pattern)?;

    let entry = ObjectRouteEntry {
        pattern: pattern.clone(),
        shape: spec.shape,
        leaves: listing_leaves,
        read: route.clone().read_handler(),
        list: route.list_handler(),
        face_handlers: spec.face_handlers.clone(),
        anchor_collections: Vec::new(),
        validator: captures_validator::<O::Key>(),
    };

    Ok(MountedObject {
        entry,
        claims: leaf_claims,
    })
}

// ===========================================================================
// Serve helpers
// ===========================================================================

struct ServeCtx<'a, O: Object> {
    render_table: &'a RenderTable,
    leaves: &'a [ObjectLeaf<O>],
    stability: Stability,
}

impl<O: Object> Clone for ServeCtx<'_, O> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<O: Object> Copy for ServeCtx<'_, O> {}

fn serve_warm<O: Object>(
    key: &O::Key,
    target: ObjectReadTarget,
    bytes: &[u8],
    validator: Option<VersionToken>,
    ctx: ServeCtx<'_, O>,
) -> Result<ReadOutcome> {
    serve_from_canonical::<O>(key, target, bytes, validator, ctx, Effects::new())
}

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
        ObjectReadTarget::Derived(name) => serve_derived(value, key, &name, ctx, effects),
        other => serve_from_canonical::<O>(key, other, bytes, validator, ctx, effects),
    }
}

fn serve_from_canonical<O: Object>(
    key: &O::Key,
    target: ObjectReadTarget,
    bytes: &[u8],
    validator: Option<VersionToken>,
    ctx: ServeCtx<'_, O>,
    effects: Effects,
) -> Result<ReadOutcome> {
    match target {
        ObjectReadTarget::Canonical => Ok(ReadOutcome::Found(
            FileContent::canonical(representation_attrs(
                Size::Unknown,
                ctx.stability,
                validator,
            ))
            .with_effects(effects),
        )),
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
        ObjectReadTarget::Derived(name) => {
            let value = O::decode(bytes)?;
            serve_derived(&value, key, &name, ctx, effects)
        },
        ObjectReadTarget::Direct(name) | ObjectReadTarget::Stream(name) => {
            Err(ProviderError::internal(format!(
                "face {name:?} must be served through its own handler, not canonical bytes"
            )))
        },
    }
}

fn serve_derived<O: Object>(
    value: &O,
    key: &O::Key,
    name: &str,
    ctx: ServeCtx<'_, O>,
    effects: Effects,
) -> Result<ReadOutcome> {
    for leaf in ctx.leaves {
        if let ObjectLeaf::Derived {
            leaf_name, derive, ..
        } = leaf
            && leaf_name == name
        {
            let content = derive(value, key)?.to_browse_content()?;
            let size = content_size(&content);
            let content = content.with_attrs(FileAttrs::new(Size::Exact(size), ctx.stability));
            return Ok(ReadOutcome::Found(content.with_effects(effects)));
        }
    }
    Err(ProviderError::not_found(format!("field {name} not found")))
}

// ===========================================================================
// Facet expansion (unchanged)
// ===========================================================================

#[derive(Clone, Debug)]
pub(super) struct FacetExpansion {
    axes: Vec<FacetExpansionAxis>,
}

impl FacetExpansion {
    pub(super) fn for_pattern<K: FacetMetadata>(pattern: &Pattern) -> Result<Self> {
        Self::for_axes(pattern, K::facet_axes())
    }

    /// Build from an explicit facet-axis slice (the child key's axes resolved
    /// at seal, where the key type is not statically in scope).
    pub(super) fn for_axes(pattern: &Pattern, axes: &[FacetAxis]) -> Result<Self> {
        let axes = axes
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

// ===========================================================================
// Collection child-view resolution (seal time)
// ===========================================================================

/// The two collection topologies, discriminated at seal by comparing the
/// collection dir pattern to the child object's registered template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CollectionTopology {
    /// The child template is strictly deeper than the collection dir
    /// (`issues/{filter}` -> `.../{number}`). The collection dir is a real dir
    /// route; child anchors live one or more segments below it.
    Nested,
    /// `PARENT/NAME == child template` (`{repo}`). The collection attaches to
    /// the parent object's anchor listing; there is no separate dir route.
    Anchor,
}

/// A collection's child object resolved against the object registry at seal
/// time: enough to compute, for each listed entry, the child anchor path, the
/// dir-entry name, and the child canonical-view leaf paths (facet-expanded).
#[derive(Clone)]
pub(crate) struct ResolvedChildView {
    /// The child object's registered template (e.g. `/{owner}/{repo}/issues/{filter}/{number}`).
    child_template: String,
    /// The child object kind, for the child's logical id.
    child_kind: ObjectKind,
    /// The child's canonical-view leaf names (canonical/representation/derived).
    child_leaf_names: Vec<String>,
    /// The child's facet expansion against its own template.
    facet_expansion: FacetExpansion,
    /// Depth (segment count) of the collection dir path (`PARENT/NAME`), used to
    /// split off the child-name segment(s) beyond the collection dir.
    dir_depth: usize,
}

impl ResolvedChildView {
    pub(super) fn new(
        child_template: String,
        child_kind: ObjectKind,
        child_leaf_names: Vec<String>,
        facet_expansion: FacetExpansion,
        dir_depth: usize,
    ) -> Self {
        Self {
            child_template,
            child_kind,
            child_leaf_names,
            facet_expansion,
            dir_depth,
        }
    }

    /// Render the child template into a concrete anchor base from a complete
    /// capture map (identity captures plus a default value for every facet
    /// segment, which facet expansion later overwrites).
    fn render_anchor_base(&self, captures: &std::collections::BTreeMap<&str, String>) -> String {
        let mut out = String::new();
        for raw in self.child_template.split('/').skip(1) {
            out.push('/');
            let segment = if let Some(name) = capture_name_of(raw) {
                captures.get(name).cloned().map_or_else(
                    || raw.to_string(),
                    |value| render_template_segment(raw, &value),
                )
            } else {
                raw.to_string()
            };
            out.push_str(&segment);
        }
        out
    }

    /// Compute the dir-entry name, child anchor base, child logical id, and
    /// facet-expanded canonical-view leaf paths for one listed entry, from its
    /// identity captures and its key's facet axes.
    pub(crate) fn entry_view(
        &self,
        identity_captures: &[(&'static str, String)],
        facet_axes: &[FacetAxis],
    ) -> Result<EntryView> {
        // A full capture map: identity captures verbatim, facets at their first
        // choice (facet expansion rewrites those segments across all choices).
        let mut captures: std::collections::BTreeMap<&str, String> = identity_captures
            .iter()
            .map(|(n, v)| (*n, v.clone()))
            .collect();
        for axis in facet_axes {
            if let Some(first) = axis.choices.first() {
                captures
                    .entry(axis.capture_name)
                    .or_insert_with(|| (*first).to_string());
            }
        }

        let anchor_base = self.render_anchor_base(&captures);

        // The dir-entry name is the segment(s) of the child anchor beyond the
        // collection dir path.
        let segments: Vec<&str> = anchor_base.split('/').skip(1).collect();
        let child_name = segments
            .get(self.dir_depth..)
            .filter(|tail| !tail.is_empty())
            .map(|tail| tail.join("/"))
            .or_else(|| segments.last().map(|s| (*s).to_string()))
            .unwrap_or_default();

        let id = crate::identity::LogicalId::new(self.child_kind, identity_captures.to_vec());

        let mut view_leaves = Vec::new();
        for leaf_name in &self.child_leaf_names {
            let leaf_path = format!("{anchor_base}/{leaf_name}");
            view_leaves.extend(self.facet_expansion.expand_view_leaves(&leaf_path)?);
        }

        Ok(EntryView {
            child_name,
            id,
            view_leaves,
            anchor_base,
        })
    }
}

/// The per-entry resolution a collection lowering needs.
pub(crate) struct EntryView {
    pub child_name: String,
    pub id: crate::identity::LogicalId,
    pub view_leaves: Vec<String>,
    pub anchor_base: String,
}

/// The capture name of a template segment (`{name}` or `prefix{name}`), or
/// `None` for a literal segment.
fn capture_name_of(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    if !raw.ends_with('}') {
        return None;
    }
    Some(&raw[start + 1..raw.len() - 1])
}

/// Render a capture template segment with its prefix preserved (`v{version}` +
/// `3` -> `v3`).
fn render_template_segment(raw: &str, value: &str) -> String {
    let start = raw.find('{').unwrap_or(0);
    let prefix = &raw[..start];
    format!("{prefix}{value}")
}

// ===========================================================================
// Small lowering helpers
// ===========================================================================

fn render_fn<O, F>() -> crate::repr::RenderFn
where
    O: Object + Representable<F>,
    F: Format,
{
    |canonical| O::decode(canonical).map(|value| value.represent())
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

// ===========================================================================
// Dispatch resolution on the mounted entry
// ===========================================================================

impl<S> ObjectRouteEntry<S> {
    /// Resolve a leaf name under this anchor to its read target by exact
    /// leaf-name match against its registered kind, or `None` if no such leaf
    /// exists. Resolution is by KIND, never by extension: a derived leaf named
    /// `notes.md` routes to its derive fn, not to a Markdown representation
    /// render that happens to share the extension. `Stream` faces resolve here
    /// too; the caller routes them to `open_file`.
    pub(super) fn read_target_for_leaf(&self, name: &str) -> Option<ObjectReadTarget> {
        let kind = self.leaf_kind(name)?;
        Some(match kind {
            LeafKind::Canonical => ObjectReadTarget::Canonical,
            LeafKind::Representation(ct) => ObjectReadTarget::Representation(ct),
            LeafKind::Derived => ObjectReadTarget::Derived(name.to_string()),
            // Object faces serve their child canonical through the same boxed
            // direct handler as a direct face.
            LeafKind::Direct | LeafKind::Object => ObjectReadTarget::Direct(name.to_string()),
            LeafKind::Stream => ObjectReadTarget::Stream(name.to_string()),
        })
    }

    /// The read target when the file-object anchor (file shape) IS read.
    pub(super) fn file_anchor_target(&self) -> Option<ObjectReadTarget> {
        if self.shape != AnchorShape::File {
            return None;
        }
        let leaf = self.leaves.first()?;
        self.read_target_for_leaf(&leaf.name)
    }

    /// The registered face kind for a leaf, by exact name match.
    fn leaf_kind(&self, name: &str) -> Option<LeafKind> {
        self.leaves
            .iter()
            .find(|leaf| leaf.name == name)
            .map(|leaf| leaf.kind)
    }

    /// Call a direct/object face's boxed handler.
    pub(super) async fn read_face(
        &self,
        cx: &Cx<S>,
        name: &str,
        caps: Captures,
    ) -> Result<FileProjection> {
        match self.face_handlers.get(name) {
            Some(FaceHandler::Direct(handler)) => handler(cx, caps).await,
            _ => Err(ProviderError::not_found(format!("face {name} not found"))),
        }
    }

    /// Open a stream face's boxed handler.
    pub(super) async fn open_face(
        &self,
        cx: &Cx<S>,
        name: &str,
        caps: Captures,
    ) -> Result<OpenedFile> {
        match self.face_handlers.get(name) {
            Some(FaceHandler::Stream(handler)) => handler(cx, caps).await,
            _ => Err(ProviderError::not_found(format!(
                "stream face {name} not found"
            ))),
        }
    }
}
