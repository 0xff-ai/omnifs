//! Mounted object dispatch, collection resolution, and read targets.

use super::super::descriptor::RouteKind;
use super::super::handlers::{RouteValidator, captures_validator};
use super::super::pattern::{CaptureLocation, Pattern};
use super::serve::ObjectRoute;
use super::spec::{AnchorShape, CollectionHandler, ComputedFn, ObjectSpec};
use crate::browse::{CachedCanonical, Effects, ReadOutcome};
use crate::captures::Captures;
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::file_attrs::{Stability, VersionToken};
use crate::handler::OpenedFile;
use crate::object::{FacetAxis, FacetMetadata, Key, Object, ObjectKind};
use crate::projection::FileProjection;
use omnifs_core::ContentType;
use std::future::Future;
use std::pin::Pin;

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
/// `Canonical`/`Representation`/`Computed` serve from the object's canonical
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
    Computed {
        leaf_name: String,
        computed: ComputedFn<O>,
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
            Self::Computed {
                leaf_name,
                computed,
                lazy,
            } => Self::Computed {
                leaf_name: leaf_name.clone(),
                computed: *computed,
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
    pub(super) fn leaf_name(&self) -> &str {
        match self {
            Self::Canonical { leaf_name, .. }
            | Self::Representation { leaf_name, .. }
            | Self::Computed { leaf_name, .. }
            | Self::Live { leaf_name, .. } => leaf_name,
        }
    }

    /// Whether this leaf is a view of the canonical bytes (canonical,
    /// representation, computed) versus an independently served face.
    pub(super) fn is_canonical_view(&self) -> bool {
        matches!(
            self,
            Self::Canonical { .. } | Self::Representation { .. } | Self::Computed { .. }
        )
    }

    /// The mount-time leaf kind for exact-name dispatch resolution.
    pub(super) fn leaf_kind(&self) -> LeafKind {
        match self {
            Self::Canonical { .. } => LeafKind::Canonical,
            Self::Representation { ct, .. } => LeafKind::Representation(*ct),
            Self::Computed { .. } => LeafKind::Computed,
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
/// The mounted, type-erased object route the dispatch tables hold.
pub(in crate::router) struct ObjectRouteEntry<S> {
    pub pattern: Pattern,
    pub kind: ObjectKind,
    pub route_kind: RouteKind,
    pub shape: AnchorShape,
    pub leaves: Vec<ListingLeaf>,
    pub read: BoxedObjectRead<S>,
    pub list: BoxedObjectList<S>,
    /// Per-leaf live-face handlers (direct/blob/stream/object), keyed by leaf
    /// name. Shared with the spec so an alias mount replays the same closures.
    pub face_handlers: std::rc::Rc<std::collections::BTreeMap<String, FaceHandler<S>>>,
    /// The ANCHOR-topology collection attached at seal: its child template
    /// equals this anchor, so it merges into this anchor's listing/lookup
    /// instead of getting a separate dir route.
    pub anchor_collection: Option<AnchorCollection<S>>,
    pub validator: RouteValidator,
}

/// An ANCHOR-topology collection attached to a parent object's anchor: the
/// boxed list handler plus the child view resolved at seal time. The parent's
/// anchor listing runs it, merges the child-name entries, and emits each fresh
/// child's canonical store.
pub(in crate::router) struct AnchorCollection<S> {
    pub handler: CollectionHandler<S>,
    pub child_view: std::rc::Rc<ResolvedChildView>,
}

impl<S> ObjectRouteEntry<S> {
    /// Run the attached ANCHOR collection and lower it to a
    /// [`crate::projection::DirProjection`], using the captures decoded from
    /// the anchor path and the host's continuation cursor.
    pub(in crate::router) async fn run_anchor_collection(
        &self,
        cx: &Cx<S>,
        caps: &Captures,
        cursor: Option<crate::handler::Cursor>,
    ) -> Result<Option<crate::projection::DirProjection>> {
        let Some(collection) = &self.anchor_collection else {
            return Ok(None);
        };
        let dir_cx =
            crate::handler::DirCx::new(cx.clone(), crate::handler::DirIntent::List { cursor });
        let projection =
            (collection.handler)(dir_cx, caps.clone(), collection.child_view.clone()).await?;
        Ok(Some(projection))
    }
}

/// A live-face handler erased over its concrete future, plus how to dispatch
/// it (read-file vs open-file).
pub(in crate::router) enum FaceHandler<S> {
    /// A direct/blob/object face: served through `read_file`.
    Direct(BoxedFaceRead<S>),
    /// A stream face: served through `open_file`.
    Stream(BoxedFaceOpen<S>),
}

pub(in crate::router) type BoxedFaceRead<S> = Box<
    dyn for<'a> Fn(
        &'a Cx<S>,
        Captures,
    ) -> Pin<Box<dyn Future<Output = Result<FileProjection>> + 'a>>,
>;
pub(in crate::router) type BoxedFaceOpen<S> = Box<
    dyn for<'a> Fn(&'a Cx<S>, Captures) -> Pin<Box<dyn Future<Output = Result<OpenedFile>> + 'a>>,
>;

/// What kind of face a listed leaf is, resolved by exact leaf-name match (not
/// by extension): this is how a computed leaf named `notes.md` routes to its
/// computed fn rather than the Markdown representation render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::router) enum LeafKind {
    Canonical,
    Representation(ContentType),
    Computed,
    Direct,
    Stream,
    Object,
}

/// A child name an object anchor lists, precomputed at mount time.
pub(in crate::router) struct ListingLeaf {
    pub name: String,
    /// The face kind this leaf resolves to (exact-name match drives dispatch).
    pub kind: LeafKind,
}

impl ListingLeaf {
    /// Whether the canonical-source leaf is stamped with the loaded byte length.
    pub(in crate::router) fn is_canonical(&self) -> bool {
        matches!(self.kind, LeafKind::Canonical)
    }

    /// Whether this leaf is a stream face (`o.file(..).stream(..)`): it opens
    /// through `open-file`/`read-chunk`, so its listing/lookup placeholder must
    /// carry `ReadMode::Ranged`, not the whole-file `ReadMode::Full` every other
    /// leaf kind uses.
    pub(in crate::router) fn is_stream(&self) -> bool {
        matches!(self.kind, LeafKind::Stream)
    }
}

/// What mounting an object spec yields: the dispatchable entry and the leaf
/// claims to feed [`Router::seal`](super::Router::seal).
pub(in crate::router) struct MountedObject<S> {
    pub entry: ObjectRouteEntry<S>,
    pub claims: Vec<Pattern>,
}

pub(in crate::router) type BoxedObjectRead<S> = Box<
    dyn for<'a> Fn(
        &'a Cx<S>,
        Captures,
        ObjectReadTarget,
        Option<CachedCanonical>,
        String,
    ) -> Pin<Box<dyn Future<Output = Result<ReadOutcome>> + 'a>>,
>;

pub(in crate::router) type BoxedObjectList<S> = Box<
    dyn for<'a> Fn(
        &'a Cx<S>,
        Captures,
        String,
    ) -> Pin<Box<dyn Future<Output = Result<ObjectListing>> + 'a>>,
>;

/// What an anchor's list dispatch needs.
pub(in crate::router) struct ObjectListing {
    pub effects: Effects,
    pub source: Option<SourceLeafAttrs>,
}

pub(in crate::router) struct SourceLeafAttrs {
    pub len: u64,
    pub validator: Option<VersionToken>,
    pub stability: Stability,
}

/// Which child of the anchor a read addresses.
pub(in crate::router) enum ObjectReadTarget {
    /// The verbatim canonical bytes.
    Canonical,
    /// A rendered representation by content type.
    Representation(ContentType),
    /// A computed field by leaf name.
    Computed(String),
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
        claims.push(Pattern::parse(&format!("{mount}/{}", leaf.leaf_name()))?);
    }
    Ok(claims)
}

/// Specialize an [`ObjectSpec`] at a concrete mount pattern.
pub(in crate::router) fn mount_object<O>(
    pattern: &Pattern,
    spec: &ObjectSpec<O>,
    combined_template: &str,
    route_kind: RouteKind,
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
        kind: O::kind(),
        route_kind,
        shape: spec.shape,
        leaves: listing_leaves,
        read: route.clone().read_handler(),
        list: route.list_handler(),
        face_handlers: spec.face_handlers.clone(),
        anchor_collection: None,
        validator: captures_validator::<O::Key>(),
    };

    Ok(MountedObject {
        entry,
        claims: leaf_claims,
    })
}
// ===========================================================================
// Facet expansion (unchanged)
// ===========================================================================

#[derive(Clone, Debug)]
pub(in crate::router) struct FacetExpansion {
    axes: Vec<FacetExpansionAxis>,
}

impl FacetExpansion {
    pub(in crate::router) fn for_pattern<K: FacetMetadata>(pattern: &Pattern) -> Result<Self> {
        Self::for_axes(pattern, K::facet_axes())
    }

    /// Build from an explicit facet-axis slice (the child key's axes resolved
    /// at seal, where the key type is not statically in scope).
    pub(in crate::router) fn for_axes(pattern: &Pattern, axes: &[FacetAxis]) -> Result<Self> {
        let axes = axes
            .iter()
            .map(|axis| FacetExpansionAxis::for_pattern(pattern, axis))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { axes })
    }

    pub(in crate::router) fn expand_view_leaves(&self, read_path: &str) -> Result<Vec<String>> {
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
pub(in crate::router) enum CollectionTopology {
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
    /// The child's canonical-view leaf names (canonical/representation/computed).
    child_leaf_names: Vec<String>,
    /// The child's facet expansion against its own template.
    facet_expansion: FacetExpansion,
    /// Depth (segment count) of the collection dir path (`PARENT/NAME`), used to
    /// split off the child-name segment(s) beyond the collection dir.
    dir_depth: usize,
}

impl ResolvedChildView {
    pub(in crate::router) fn new(
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
// Dispatch resolution on the mounted entry
// ===========================================================================

impl<S> ObjectRouteEntry<S> {
    /// Resolve a leaf name under this anchor to its read target by exact
    /// leaf-name match against its registered kind, or `None` if no such leaf
    /// exists. Resolution is by KIND, never by extension: a computed leaf named
    /// `notes.md` routes to its computed fn, not to a Markdown representation
    /// render that happens to share the extension. `Stream` faces resolve here
    /// too; the caller routes them to `open_file`.
    pub(in crate::router) fn read_target_for_leaf(&self, name: &str) -> Option<ObjectReadTarget> {
        let kind = self.leaf_kind(name)?;
        Some(match kind {
            LeafKind::Canonical => ObjectReadTarget::Canonical,
            LeafKind::Representation(ct) => ObjectReadTarget::Representation(ct),
            LeafKind::Computed => ObjectReadTarget::Computed(name.to_string()),
            // Object faces serve their child canonical through the same boxed
            // direct handler as a direct face.
            LeafKind::Direct | LeafKind::Object => ObjectReadTarget::Direct(name.to_string()),
            LeafKind::Stream => ObjectReadTarget::Stream(name.to_string()),
        })
    }

    /// The read target when the file-object anchor (file shape) IS read.
    pub(in crate::router) fn file_anchor_target(&self) -> Option<ObjectReadTarget> {
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
    pub(in crate::router) async fn read_face(
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
    pub(in crate::router) async fn open_face(
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
