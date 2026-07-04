//! Route registration and object mounting.
//!
//! Everything here runs once, inside a provider's `start`. The route table is
//! append-only and immutable after [`Router::seal`]; dispatch (the
//! `dispatch` submodule) reads it on every host browse call.

use crate::error::{ProviderError, Result};
use crate::object::{FacetMetadata, Key, Object};
use std::collections::BTreeSet;

use super::descriptor::{RouteDescriptor, RouteKind};
use super::handlers::{IntoDirHandler, IntoFileHandler, IntoTreeRefHandler};
use super::object::{ObjectBlock, ObjectHandle, mount_object, object};
use super::pattern::{Pattern, parse_pattern};

// ===========================================================================
// Router
// ===========================================================================

/// The registration and dispatch surface; `S` is the provider state type
/// handlers receive through their `Cx<S>` / `DirCx<S>`.
///
/// Routes live in per-kind tables (dirs, files, treerefs, objects).
/// Separately, `leaf_claims` accumulates one pattern per leaf so
/// [`Self::seal`] can enforce one-path-one-route across all kinds at once,
/// and `object_registry` records each mounted object kind so a collection
/// face can resolve its child object's anchor at seal time.
///
/// ```ignore
/// fn start(config: Config, r: &mut Router<State>) -> Result<State> {
///     r.dir("/").handler(root_list)?;
///     r.file("/rate_limit").handler(read_rate_limit)?;   // literal beats {owner}
///     r.object::<Repo>("/{owner}/{repo}", |o| {
///         o.dynamic();
///         o.file("repo.json").canonical::<Json>()?;
///         o.dir("repo").tree(Repo::tree)?;
///         Ok(())
///     })?;
///     Ok(State::default())
/// }
/// ```
pub struct Router<S = ()> {
    pub(super) dirs: Vec<super::handlers::DirEntry<S>>,
    pub(super) files: Vec<super::handlers::FileEntry<S>>,
    pub(super) treerefs: Vec<super::handlers::TreeRefEntry<S>>,
    pub(super) objects: Vec<super::object::ObjectRouteEntry<S>>,
    pub(super) leaf_claims: Vec<Pattern>,
    pub(super) object_registry: Vec<RegisteredObject>,
    pub(super) route_descriptors: Vec<RouteDescriptor>,
    /// Collection faces declared on object dir faces, resolved against the
    /// registry at seal time (which registers the dir route for NESTED
    /// collections and attaches ANCHOR collections to the parent object).
    pub(super) collections: Vec<CollectionRef<S>>,
}

/// Registry record for one mounted object kind, used by collection faces to
/// resolve the child object's anchor template, view leaves, and facet axes at
/// seal time.
pub(super) struct RegisteredObject {
    pub kind_str: &'static str,
    pub template: String,
    pub has_canonical: bool,
    /// The object's canonical-view leaf names (canonical/representation/derived).
    pub canonical_view_leaf_names: Vec<String>,
    /// The object key's facet axes, for child view-leaf expansion.
    pub facet_axes: &'static [crate::object::FacetAxis],
}

/// A collection face awaiting registry resolution at seal time. Carries the
/// late-bound child-view cell its handler reads, plus, for the NESTED
/// topology, the boxed list handler and validator deferred for dir-route
/// registration (see [`Router::seal`]).
pub(super) struct CollectionRef<S> {
    /// The collection dir path (`parent_anchor/name`).
    pub dir_path: String,
    /// The parent object's template (`dir_path` minus the face name).
    pub parent_template: String,
    pub child_kind_str: &'static str,
    /// Whether the collection can emit `fresh` entries (always true for the
    /// typed `collection::<C>` form).
    pub requires_canonical: bool,
    /// The late-bound child view this collection's handler reads.
    pub late_view: super::object::LateChildView,
    /// The boxed collection list handler.
    pub handler: super::object::CollectionHandler<S>,
    /// Per-route capture validator for the deferred dir route.
    pub validator: super::handlers::RouteValidator,
}

impl<S> Default for Router<S> {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            files: Vec::new(),
            treerefs: Vec::new(),
            objects: Vec::new(),
            leaf_claims: Vec::new(),
            object_registry: Vec::new(),
            route_descriptors: Vec::new(),
            collections: Vec::new(),
        }
    }
}

impl<S> Router<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a directory route at `template`; finish with
    /// [`DirRoute::handler`].
    pub fn dir(&mut self, template: &'static str) -> DirRoute<'_, S> {
        DirRoute {
            router: self,
            template,
        }
    }

    /// Begin a file route at `template`; finish with [`FileRoute::handler`].
    pub fn file(&mut self, template: &'static str) -> FileRoute<'_, S> {
        FileRoute {
            router: self,
            template,
            ranged: false,
        }
    }

    /// Begin a subtree-handoff route at `template`; finish with
    /// [`TreeRefRoute::handler`].
    pub fn treeref(&mut self, template: &'static str) -> TreeRefRoute<'_, S> {
        TreeRefRoute {
            router: self,
            template,
        }
    }

    /// Bind a dir-shaped [`Object`] at `template`: define and mount in one
    /// call. The anchor path becomes a directory whose children are the faces
    /// declared in `block`. Returns an [`ObjectHandle`] that can be aliased at
    /// another template with [`Self::alias`].
    pub fn object<O>(
        &mut self,
        template: &'static str,
        block: impl FnOnce(&mut ObjectBlock<O>) -> Result<()>,
    ) -> Result<ObjectHandle<O>>
    where
        O::Key: Key + FacetMetadata + 'static,
        O::State: 'static,
        S: 'static,
        O: Object<State = S> + 'static,
    {
        let handle = object(template, block)?;
        self.mount_handle(template, &handle, RouteKind::Object)?;
        Ok(handle)
    }

    /// Bind a file-shaped [`Object`] at `template`: the anchor projects as a
    /// single file (one canonical/representation/direct/blob face), not a
    /// directory.
    pub fn file_object<O>(
        &mut self,
        template: &'static str,
        block: impl FnOnce(&mut ObjectBlock<O>) -> Result<()>,
    ) -> Result<ObjectHandle<O>>
    where
        O::Key: Key + FacetMetadata + 'static,
        O::State: 'static,
        S: 'static,
        O: Object<State = S> + 'static,
    {
        let handle = super::object::file_object(template, block)?;
        self.mount_handle(template, &handle, RouteKind::FileObject)?;
        Ok(handle)
    }

    /// Mount the same object spec at another `template` (an alias). Captures
    /// must satisfy the key (checked at seal time).
    pub fn alias<O>(
        &mut self,
        template: &'static str,
        handle: &ObjectHandle<O>,
    ) -> Result<&mut Self>
    where
        O::Key: Key + FacetMetadata + 'static,
        O::State: 'static,
        S: 'static,
        O: Object<State = S> + 'static,
    {
        self.mount_handle(template, handle, RouteKind::Alias)?;
        Ok(self)
    }

    fn mount_handle<O>(
        &mut self,
        template: &'static str,
        handle: &ObjectHandle<O>,
        route_kind: RouteKind,
    ) -> Result<()>
    where
        O::Key: Key + FacetMetadata + 'static,
        O::State: 'static,
        S: 'static,
        O: Object<State = S> + 'static,
    {
        if !template.starts_with('/') {
            return Err(ProviderError::invalid_input(format!(
                "object template must be absolute: {template}"
            )));
        }
        let pattern = parse_pattern(template)?;
        let mounted = mount_object::<O>(&pattern, handle.spec.as_ref(), template, route_kind)?;
        self.object_registry.push(RegisteredObject {
            kind_str: O::kind().as_str(),
            template: template.to_string(),
            has_canonical: handle.has_canonical(),
            canonical_view_leaf_names: handle.canonical_view_leaf_names(),
            facet_axes: <O::Key as FacetMetadata>::facet_axes(),
        });
        // Defer collection dir-route registration to seal: the child object's
        // template, leaves, and facet axes may not be registered yet, and the
        // ANCHOR topology (child template == collection dir) must NOT get a
        // separate dir route. Pair each declared collection with its boxed
        // handler and the validator for the deferred dir route.
        let handlers = handle.collection_handlers();
        for decl in handle.collection_decls() {
            let Some(entry) = handlers
                .iter()
                .find(|entry| entry.dir_path == decl.dir_path)
            else {
                return Err(ProviderError::internal(format!(
                    "collection at {} has no generated list handler",
                    decl.dir_path
                )));
            };
            self.collections.push(CollectionRef {
                dir_path: decl.dir_path.clone(),
                parent_template: decl.parent_template.clone(),
                child_kind_str: decl.child_kind_str,
                requires_canonical: decl.requires_canonical,
                late_view: entry.late_view.clone(),
                handler: entry.handler.clone(),
                validator: entry.validator.clone(),
            });
        }
        // Register each tree face as a treeref route at `template/name`,
        // claiming that path exactly once (the treeref registration claims it;
        // the tree face itself does not). A lookup/list there returns the
        // subtree handoff.
        let mount = template.trim_end_matches('/');
        for face in handle.tree_faces() {
            let tree_path = format!("{mount}/{}", face.name);
            let pattern = parse_pattern(&tree_path)?;
            self.treerefs.push(super::handlers::TreeRefEntry {
                pattern: pattern.clone(),
                handler: face.handler.clone(),
                validator: face.validator.clone(),
            });
            self.leaf_claims.push(pattern);
        }

        // Register each choices face as an exhaustive dir route at
        // `template/name`, so a readdir lists exactly the fixed names. The dir
        // route claims the path (the choices face does not).
        for face in handle.choices_faces() {
            let choices_path = format!("{mount}/{}", face.name);
            let pattern = parse_pattern(&choices_path)?;
            let names = face.names;
            let handler: super::handlers::BoxedDirHandler<S> =
                std::sync::Arc::new(move |_dir_cx, _caps| {
                    let entries = names
                        .iter()
                        .map(|name| crate::projection::Entry::dir(*name))
                        .collect::<Vec<_>>();
                    Box::pin(
                        async move { Ok(crate::projection::DirProjection::exhaustive(entries)) },
                    )
                });
            self.dirs.push(super::handlers::DirEntry {
                pattern: pattern.clone(),
                handler,
                validator: super::handlers::accept_validator(),
            });
            self.leaf_claims.push(pattern);
        }

        self.objects.push(mounted.entry);
        self.leaf_claims.extend(mounted.claims);
        Ok(())
    }

    fn dir_at<Marker, H: IntoDirHandler<S, Marker>>(&mut self, template: &str, h: H) -> Result<()> {
        let pattern = parse_pattern(template)?;
        let (handler, validator) = h.into_dir_handler();
        self.dirs.push(super::handlers::DirEntry {
            pattern: pattern.clone(),
            handler,
            validator,
        });
        self.leaf_claims.push(pattern);
        Ok(())
    }

    fn file_at<Marker, H: IntoFileHandler<S, Marker>>(
        &mut self,
        template: &str,
        ranged: bool,
        h: H,
    ) -> Result<()> {
        let pattern = parse_pattern(template)?;
        let (handler, validator) = h.into_file_handler();
        self.files.push(super::handlers::FileEntry {
            pattern: pattern.clone(),
            handler,
            validator,
            ranged,
        });
        self.leaf_claims.push(pattern);
        Ok(())
    }

    fn treeref_at<Marker, H: IntoTreeRefHandler<S, Marker>>(
        &mut self,
        template: &str,
        h: H,
    ) -> Result<()> {
        let pattern = parse_pattern(template)?;
        let (handler, validator) = h.into_treeref_handler();
        self.treerefs.push(super::handlers::TreeRefEntry {
            pattern: pattern.clone(),
            handler,
            validator,
        });
        self.leaf_claims.push(pattern);
        Ok(())
    }

    /// Seal-time validation and collection resolution, called by the
    /// `#[omnifs_sdk::provider]` glue after `start` returns; providers do not
    /// call it themselves.
    ///
    /// Resolves every collection against the object registry (now that all
    /// routes are known): computes the child's view template (child template,
    /// canonical-view leaves, facet axes), discriminates the NESTED vs ANCHOR
    /// topology, and either registers a dir route (NESTED) or attaches the
    /// collection to the parent object's anchor (ANCHOR). Then runs the
    /// one-path-one-route overlap check over all leaf claims.
    ///
    /// The face-level checks (canonical CT, single canonical, representation
    /// without canonical, Live only on stream, reserved `@`) run at build time
    /// inside the block builder and need no re-check here.
    pub fn seal(&mut self) -> Result<()>
    where
        S: 'static,
    {
        let collection_descriptors = self.resolve_collections()?;

        for (i, left) in self.leaf_claims.iter().enumerate() {
            for right in self.leaf_claims.iter().skip(i + 1) {
                if left.is_ambiguous_with(right) {
                    return Err(ProviderError::invalid_input(format!(
                        "overlapping routes: {} vs {}",
                        left.parent_signature(),
                        right.parent_signature()
                    )));
                }
            }
        }

        self.route_descriptors = self.describe_routes(collection_descriptors);
        Ok(())
    }

    /// Return the route descriptors captured when this router was sealed.
    #[must_use]
    pub fn routes(&self) -> Vec<RouteDescriptor> {
        self.route_descriptors.clone()
    }

    fn describe_routes(
        &self,
        collection_descriptors: Vec<RouteDescriptor>,
    ) -> Vec<RouteDescriptor> {
        let collection_templates = collection_descriptors
            .iter()
            .map(|descriptor| descriptor.template.clone())
            .collect::<BTreeSet<_>>();
        let mut routes = Vec::new();
        routes.extend(
            self.dirs
                .iter()
                .filter(|entry| !collection_templates.contains(&entry.pattern.template()))
                .map(|entry| {
                    RouteDescriptor::new(
                        &entry.pattern,
                        RouteKind::Dir,
                        None,
                        entry.validator.capture_descriptors(),
                    )
                }),
        );
        routes.extend(self.files.iter().map(|entry| {
            RouteDescriptor::new(
                &entry.pattern,
                RouteKind::File,
                None,
                entry.validator.capture_descriptors(),
            )
        }));
        routes.extend(self.treerefs.iter().map(|entry| {
            RouteDescriptor::new(
                &entry.pattern,
                RouteKind::Treeref,
                None,
                entry.validator.capture_descriptors(),
            )
        }));
        routes.extend(self.objects.iter().map(|entry| {
            RouteDescriptor::new(
                &entry.pattern,
                entry.route_kind,
                Some(entry.kind_str.to_string()),
                entry.validator.capture_descriptors(),
            )
        }));
        routes.extend(collection_descriptors);
        routes
    }

    /// Resolve every declared collection against the object registry, fill its
    /// late-bound child view, discriminate topology, and wire it: a NESTED
    /// collection becomes a dir route at the collection path; an ANCHOR
    /// collection attaches to the parent object's anchor listing.
    fn resolve_collections(&mut self) -> Result<Vec<RouteDescriptor>>
    where
        S: 'static,
    {
        let collections = std::mem::take(&mut self.collections);
        let mut descriptors = Vec::new();
        for collection in collections {
            let Some(child) = self
                .object_registry
                .iter()
                .find(|object| object.kind_str == collection.child_kind_str)
            else {
                return Err(ProviderError::invalid_input(format!(
                    "collection at {} references object kind {:?}, which is not registered as an r.object route",
                    collection.dir_path, collection.child_kind_str
                )));
            };
            if collection.requires_canonical && !child.has_canonical {
                return Err(ProviderError::invalid_input(format!(
                    "collection at {} emits fresh entries but child object {:?} ({}) has no canonical face",
                    collection.dir_path, collection.child_kind_str, child.template
                )));
            }

            let child_pattern = parse_pattern(&child.template)?;
            // The child key must be derivable from the child template captures
            // (Part 8): every facet axis must name a capture in the template.
            for axis in child.facet_axes {
                if child_pattern.capture_location(axis.capture_name).is_none() {
                    return Err(ProviderError::invalid_input(format!(
                        "collection at {}: child object {:?} facet {:?} is not a capture in its template {}",
                        collection.dir_path,
                        collection.child_kind_str,
                        axis.capture_name,
                        child.template
                    )));
                }
            }
            let facet_expansion =
                super::object::FacetExpansion::for_axes(&child_pattern, child.facet_axes)?;

            let dir_pattern = parse_pattern(&collection.dir_path)?;
            let dir_depth = dir_pattern.pattern_len();
            descriptors.push(RouteDescriptor::new(
                &dir_pattern,
                RouteKind::Collection,
                Some(collection.child_kind_str.to_string()),
                collection.validator.capture_descriptors(),
            ));
            // ANCHOR when the collection dir path equals the child template;
            // NESTED when the child template is strictly deeper.
            let topology = if collection.dir_path == child.template {
                super::object::CollectionTopology::Anchor
            } else if child_pattern.pattern_len() > dir_depth {
                super::object::CollectionTopology::Nested
            } else {
                return Err(ProviderError::invalid_input(format!(
                    "collection at {}: child object {:?} template {} is not the collection path nor strictly deeper",
                    collection.dir_path, collection.child_kind_str, child.template
                )));
            };

            let child_view = std::rc::Rc::new(super::object::ResolvedChildView::new(
                child.template.clone(),
                crate::object::ObjectKind(child.kind_str),
                child.canonical_view_leaf_names.clone(),
                facet_expansion,
                dir_depth,
            ));
            *collection.late_view.borrow_mut() = Some(child_view.clone());

            match topology {
                super::object::CollectionTopology::Nested => {
                    // Register the SDK-generated collection dir handler as a dir
                    // route so a readdir of the collection path runs the typed
                    // list method against the resolved child view.
                    let handler = collection.handler.clone();
                    let view = child_view.clone();
                    let boxed: super::handlers::BoxedDirHandler<S> =
                        std::sync::Arc::new(move |dir_cx, caps| {
                            handler(dir_cx, caps, view.clone())
                        });
                    self.dirs.push(super::handlers::DirEntry {
                        pattern: dir_pattern,
                        handler: boxed,
                        validator: collection.validator,
                    });
                },
                super::object::CollectionTopology::Anchor => {
                    // Attach to the parent object's anchor: its listing/lookup
                    // runs this collection and merges the child-name entries.
                    let parent_pattern = parse_pattern(&collection.parent_template)?;
                    let Some(parent) = self
                        .objects
                        .iter_mut()
                        .find(|entry| entry.pattern == parent_pattern)
                    else {
                        return Err(ProviderError::internal(format!(
                            "anchor collection at {} has no registered parent object at {}",
                            collection.dir_path, collection.parent_template
                        )));
                    };
                    parent
                        .anchor_collections
                        .push(super::object::AnchorCollection {
                            handler: collection.handler,
                            child_view,
                        });
                },
            }
        }
        Ok(descriptors)
    }
}

/// A pending [`Router::dir`] registration.
pub struct DirRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: &'static str,
}

/// A pending [`Router::file`] registration.
pub struct FileRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: &'static str,
    pub(super) ranged: bool,
}

/// A pending [`Router::treeref`] registration.
pub struct TreeRefRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: &'static str,
}

impl<'r, S> DirRoute<'r, S> {
    /// Register the directory handler and claim the template as a leaf.
    pub fn handler<Marker, H: IntoDirHandler<S, Marker>>(self, h: H) -> Result<&'r mut Router<S>> {
        self.router.dir_at(self.template, h)?;
        Ok(self.router)
    }
}

impl<'r, S> FileRoute<'r, S> {
    /// Declare that this route streams its content through the
    /// `open-file`/`read-chunk` session (`ReadMode::Ranged`).
    #[must_use]
    pub fn ranged(mut self) -> Self {
        self.ranged = true;
        self
    }

    /// Register the file handler and claim the template as a leaf.
    pub fn handler<Marker, H: IntoFileHandler<S, Marker>>(self, h: H) -> Result<&'r mut Router<S>> {
        self.router.file_at(self.template, self.ranged, h)?;
        Ok(self.router)
    }
}

impl<'r, S> TreeRefRoute<'r, S> {
    /// Register the subtree-handoff handler and claim the template as a leaf.
    pub fn handler<Marker, H: IntoTreeRefHandler<S, Marker>>(
        self,
        h: H,
    ) -> Result<&'r mut Router<S>> {
        self.router.treeref_at(self.template, h)?;
        Ok(self.router)
    }
}
