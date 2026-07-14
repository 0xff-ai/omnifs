//! Route registration and object mounting.
//!
//! Everything here runs once, inside a provider's `start`. The route table is
//! append-only. [`Router::compile`] consumes that registration state and
//! produces the immutable route table used for host browse calls.

use crate::captures::CaptureDescriptor;
use crate::error::{ProviderError, Result};
use crate::object::{FacetAxis, FacetMetadata, Key, Object, ObjectKind};
use crate::projection::FileProjection;
use omnifs_core::ContentType;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::compiled::CompiledRouter;
use super::descriptor::{RouteDescriptor, RouteKind};
use super::handlers::{IntoDirHandler, IntoFileHandler, IntoTreeRefHandler};
use super::object::{CollectionHandler, ObjectBlock, ObjectHandle, mount_object, object};
use super::pattern::Pattern;
use super::readme::{ObjectLeaves, Readme, Scope};

fn validate_route_captures(
    pattern: &Pattern,
    validator: &super::handlers::RouteValidator,
) -> Result<()> {
    let route = pattern.capture_descriptors(validator.capture_descriptors());
    let typed = validator.capture_descriptors();
    let mut route_names = route
        .iter()
        .map(|capture| capture.name.as_str())
        .collect::<Vec<_>>();
    let mut required_names = typed
        .iter()
        .filter(|capture| capture.required)
        .map(|capture| capture.name.as_str())
        .collect::<Vec<_>>();
    route_names.sort_unstable();
    required_names.sort_unstable();
    if required_names
        .iter()
        .any(|name| !route_names.contains(name))
    {
        return Err(ProviderError::invalid_input(format!(
            "required capture declaration mismatch for {}: route {:?}, handler {:?}",
            pattern.template(),
            route_names,
            required_names
        )));
    }
    Ok(())
}

/// Registration-only facts needed to validate and describe one mounted object
/// face. The immutable dispatch entry retains only facts needed after compile.
struct ObjectMountMetadata {
    pattern: Pattern,
    kind: ObjectKind,
    route_kind: RouteKind,
    facet_axes: &'static [FacetAxis],
    canonical_view_leaf_names: Vec<String>,
    capture_descriptors: Vec<CaptureDescriptor>,
}

/// A collection declaration specialized to one mounted object face. It is
/// consumed during compilation and never enters the executable route graph.
struct CollectionMount<S> {
    dir_path: String,
    parent_template: String,
    child_kind: ObjectKind,
    requires_canonical: bool,
    handler: CollectionHandler<S>,
    validator: super::handlers::RouteValidator,
}

// ===========================================================================
// Router
// ===========================================================================

/// The mutable registration builder; `S` is the provider state type handlers
/// receive through their `Cx<S>` / `DirCx<S>`.
///
/// Routes live in per-kind tables (dirs, files, treerefs, objects). Compilation
/// derives cross-kind claims and resolves collection faces against the mounted
/// object entries.
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
    collections: Vec<CollectionMount<S>>,
    object_metadata: Vec<ObjectMountMetadata>,
}

impl<S> Default for Router<S> {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            files: Vec::new(),
            treerefs: Vec::new(),
            objects: Vec::new(),
            collections: Vec::new(),
            object_metadata: Vec::new(),
        }
    }
}

impl<S> Router<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a directory route at `template`; finish with
    /// [`DirRoute::handler`]. The template may be a borrowed literal or an
    /// owned `String`.
    pub fn dir(&mut self, template: impl Into<String>) -> DirRoute<'_, S> {
        DirRoute {
            router: self,
            template: template.into(),
        }
    }

    /// Begin a file route at `template`; finish with [`FileRoute::handler`].
    /// The template may be a borrowed literal or an owned `String`.
    pub fn file(&mut self, template: impl Into<String>) -> FileRoute<'_, S> {
        FileRoute {
            router: self,
            template: template.into(),
            ranged: false,
        }
    }

    /// Begin a subtree-handoff route at `template`; finish with
    /// [`TreeRefRoute::handler`]. The template may be a borrowed literal or an
    /// owned `String`.
    pub fn treeref(&mut self, template: impl Into<String>) -> TreeRefRoute<'_, S> {
        TreeRefRoute {
            router: self,
            template: template.into(),
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
    /// must satisfy the key (checked at compile time).
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
        let pattern = Pattern::parse(template)?;
        let entry = mount_object::<O>(&pattern, handle.definition.as_ref())?;
        let mount = template.trim_end_matches('/');
        for declaration in handle.collections() {
            self.collections.push(CollectionMount {
                dir_path: format!("{mount}/{}", declaration.name),
                parent_template: template.to_string(),
                child_kind: declaration.child_kind,
                requires_canonical: declaration.requires_canonical,
                handler: declaration.handler.clone(),
                validator: declaration.validator.clone(),
            });
        }
        // Register each tree face as a treeref route at `template/name`,
        // claiming that path exactly once (the treeref registration claims it;
        // the tree face itself does not). A lookup/list there returns the
        // subtree handoff.
        for face in handle.tree_faces() {
            let tree_path = format!("{mount}/{}", face.name);
            let pattern = Pattern::parse(&tree_path)?;
            self.treerefs.push(super::handlers::TreeRefEntry {
                pattern: pattern.clone(),
                handler: face.handler.clone(),
                validator: face.validator.clone(),
            });
        }

        // Register each choices face as an exhaustive dir route at
        // `template/name`, so a readdir lists exactly the fixed names. The dir
        // route claims the path (the choices face does not).
        for face in handle.choices_faces() {
            let choices_path = format!("{mount}/{}", face.name);
            let pattern = Pattern::parse(&choices_path)?;
            let names = face.names;
            let handler: super::handlers::BoxedDirHandler<S> =
                std::sync::Arc::new(move |_dir_cx, _caps| {
                    let entries = names
                        .iter()
                        .map(|name| crate::projection::Entry::dir(*name))
                        .collect::<Vec<_>>();
                    Box::pin(async move { Ok(crate::projection::DirListing::exhaustive(entries)) })
                });
            self.dirs.push(super::handlers::DirEntry {
                pattern: pattern.clone(),
                handler,
                validator: super::handlers::accept_validator(),
            });
        }

        self.object_metadata.push(ObjectMountMetadata {
            pattern,
            kind: O::kind(),
            route_kind,
            facet_axes: <O::Key as FacetMetadata>::facet_axes(),
            canonical_view_leaf_names: handle.canonical_view_leaf_names(),
            capture_descriptors: super::handlers::captures_validator::<O::Key>()
                .capture_descriptors()
                .to_vec(),
        });
        self.objects.push(entry);
        Ok(())
    }

    fn dir_at<Marker, H: IntoDirHandler<S, Marker>>(&mut self, template: &str, h: H) -> Result<()> {
        let pattern = Pattern::parse(template)?;
        let (handler, validator) = h.into_dir_handler();
        self.dirs.push(super::handlers::DirEntry {
            pattern: pattern.clone(),
            handler,
            validator,
        });
        Ok(())
    }

    fn file_at<Marker, H: IntoFileHandler<S, Marker>>(
        &mut self,
        template: &str,
        ranged: bool,
        h: H,
    ) -> Result<()> {
        let pattern = Pattern::parse(template)?;
        let (handler, validator) = h.into_file_handler();
        self.files.push(super::handlers::FileEntry {
            pattern: pattern.clone(),
            handler,
            validator,
            ranged,
        });
        Ok(())
    }

    fn treeref_at<Marker, H: IntoTreeRefHandler<S, Marker>>(
        &mut self,
        template: &str,
        h: H,
    ) -> Result<()> {
        let pattern = Pattern::parse(template)?;
        let (handler, validator) = h.into_treeref_handler();
        self.treerefs.push(super::handlers::TreeRefEntry {
            pattern: pattern.clone(),
            handler,
            validator,
        });
        Ok(())
    }

    /// Consume the mutable declarations, validate their cross-route
    /// invariants, and produce the immutable graph used by dispatch.
    ///
    /// The face-level checks (canonical CT, single canonical, representation
    /// without canonical, Live only on stream, reserved `@`) run at build time
    /// inside the block builder and need no re-check here.
    pub fn compile(mut self) -> Result<CompiledRouter<S>>
    where
        S: 'static,
    {
        self.validate_capture_compatibility()?;
        let collection_descriptors = self.resolve_collections()?;
        let claims = self.route_claims()?;

        for (i, left) in claims.iter().enumerate() {
            for right in claims.iter().skip(i + 1) {
                if left.is_ambiguous_with(right) {
                    return Err(ProviderError::invalid_input(format!(
                        "overlapping routes: {} vs {}",
                        left.parent_signature(),
                        right.parent_signature()
                    )));
                }
            }
        }

        let route_descriptors = self.describe_routes(collection_descriptors);
        self.synthesize_readme_routes(&route_descriptors, &claims)?;
        Ok(CompiledRouter::new(
            self.dirs,
            self.files,
            self.treerefs,
            self.objects,
            route_descriptors,
        ))
    }

    fn route_claims(&self) -> Result<Vec<Pattern>> {
        let mut claims = Vec::new();
        claims.extend(self.dirs.iter().map(|entry| entry.pattern.clone()));
        claims.extend(self.files.iter().map(|entry| entry.pattern.clone()));
        claims.extend(self.treerefs.iter().map(|entry| entry.pattern.clone()));
        for entry in &self.objects {
            claims.push(entry.pattern.clone());
            if entry.shape == super::object::AnchorShape::Dir {
                for leaf in &entry.leaves {
                    claims.push(Pattern::parse(&format!(
                        "{}/{}",
                        entry.pattern.template().trim_end_matches('/'),
                        leaf.name
                    ))?);
                }
            }
        }
        Ok(claims)
    }

    fn validate_capture_compatibility(&self) -> Result<()> {
        for entry in &self.dirs {
            validate_route_captures(&entry.pattern, &entry.validator)?;
        }
        for entry in &self.files {
            validate_route_captures(&entry.pattern, &entry.validator)?;
        }
        for entry in &self.treerefs {
            validate_route_captures(&entry.pattern, &entry.validator)?;
        }
        for entry in &self.objects {
            validate_route_captures(&entry.pattern, &entry.validator)?;
        }
        Ok(())
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
        routes.extend(self.object_metadata.iter().map(|object| {
            RouteDescriptor::new(
                &object.pattern,
                object.route_kind,
                Some(object.kind.as_str().to_string()),
                &object.capture_descriptors,
            )
        }));
        routes.extend(collection_descriptors);
        routes
    }

    fn synthesize_readme_routes(
        &mut self,
        routes: &[RouteDescriptor],
        claims: &[Pattern],
    ) -> Result<()> {
        let object_leaves = self
            .object_metadata
            .iter()
            .map(|object| ObjectLeaves {
                template: object.pattern.template(),
                leaf_names: object.canonical_view_leaf_names.clone(),
            })
            .collect::<Vec<_>>();
        let mut scopes = vec![Scope::Root];
        scopes.extend(super::readme::branch_scopes(routes));
        for scope in scopes {
            let path = scope.readme_path();
            if claims.iter().any(|claim| claim.template() == path) {
                continue;
            }
            let body = Readme::new(scope, routes, &object_leaves).render();
            self.synthesize_readme_route(&path, body)?;
        }
        Ok(())
    }

    fn synthesize_readme_route(&mut self, path: &str, body: String) -> Result<()> {
        let pattern = Pattern::parse(path)?;
        let bytes = body.into_bytes();
        let handler: super::handlers::BoxedFileHandler<S> = Arc::new(move |_cx, _caps| {
            let bytes = bytes.clone();
            Box::pin(
                async move { Ok(FileProjection::body_with_type(bytes, ContentType::Markdown)) },
            )
        });
        self.files.push(super::handlers::FileEntry {
            pattern: pattern.clone(),
            handler,
            validator: super::handlers::accept_validator(),
            ranged: false,
        });
        Ok(())
    }

    /// Resolve every mounted collection against the object entries and lower it
    /// directly into the final nested or anchor route shape.
    fn resolve_collections(&mut self) -> Result<Vec<RouteDescriptor>>
    where
        S: 'static,
    {
        std::mem::take(&mut self.collections)
            .into_iter()
            .map(|collection| self.resolve_collection(collection))
            .collect()
    }

    fn resolve_collection(&mut self, collection: CollectionMount<S>) -> Result<RouteDescriptor>
    where
        S: 'static,
    {
        let Some(child) = self
            .object_metadata
            .iter()
            .find(|object| object.kind == collection.child_kind)
        else {
            return Err(ProviderError::invalid_input(format!(
                "collection at {} references object kind {:?}, which is not registered as an r.object route",
                collection.dir_path, collection.child_kind
            )));
        };
        if collection.requires_canonical && child.canonical_view_leaf_names.is_empty() {
            return Err(ProviderError::invalid_input(format!(
                "collection at {} emits fresh entries but child object {:?} ({}) has no canonical face",
                collection.dir_path,
                collection.child_kind,
                child.pattern.template()
            )));
        }

        let child_template = child.pattern.template();
        let child_pattern = child.pattern.clone();
        for axis in child.facet_axes {
            if child_pattern.capture_location(axis.capture_name).is_none() {
                return Err(ProviderError::invalid_input(format!(
                    "collection at {}: child object {:?} facet {:?} is not a capture in its template {}",
                    collection.dir_path, collection.child_kind, axis.capture_name, child_template
                )));
            }
        }
        let facet_expansion =
            super::object::FacetExpansion::for_axes(&child_pattern, child.facet_axes)?;

        let dir_pattern = Pattern::parse(&collection.dir_path)?;
        validate_route_captures(&dir_pattern, &collection.validator)?;
        let dir_depth = dir_pattern.pattern_len();
        let descriptor = RouteDescriptor::new(
            &dir_pattern,
            RouteKind::Collection,
            Some(collection.child_kind.as_str().to_string()),
            collection.validator.capture_descriptors(),
        );
        let topology = if collection.dir_path == child_template {
            super::object::CollectionTopology::Anchor
        } else if child_pattern.pattern_len() > dir_depth {
            super::object::CollectionTopology::Nested
        } else {
            return Err(ProviderError::invalid_input(format!(
                "collection at {}: child object {:?} template {} is not the collection path nor strictly deeper",
                collection.dir_path, collection.child_kind, child_template
            )));
        };

        let child_view = std::rc::Rc::new(super::object::ResolvedChildView::new(
            child_template.clone(),
            child.kind,
            child.canonical_view_leaf_names.clone(),
            facet_expansion,
            dir_depth,
        ));
        match topology {
            super::object::CollectionTopology::Nested => {
                let handler = collection.handler.clone();
                let view = child_view.clone();
                let boxed: super::handlers::BoxedDirHandler<S> =
                    std::sync::Arc::new(move |dir_cx, caps| {
                        let fut = handler(dir_cx, caps, view.clone());
                        Box::pin(async move {
                            fut.await
                                .map(crate::projection::DirListing::from_projection)
                        })
                    });
                self.dirs.push(super::handlers::DirEntry {
                    pattern: dir_pattern,
                    handler: boxed,
                    validator: collection.validator,
                });
            },
            super::object::CollectionTopology::Anchor => {
                let parent_pattern = Pattern::parse(&collection.parent_template)?;
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
                if parent.anchor_collection.is_some() {
                    return Err(ProviderError::invalid_input(format!(
                        "anchor collection at {}: parent object anchor {} already has a collection owner",
                        collection.dir_path, collection.parent_template
                    )));
                }
                parent.anchor_collection = Some(super::object::AnchorCollection {
                    handler: collection.handler,
                    child_view,
                });
            },
        }
        Ok(descriptor)
    }
}

/// A pending [`Router::dir`] registration.
pub struct DirRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: String,
}

/// A pending [`Router::file`] registration.
pub struct FileRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: String,
    pub(super) ranged: bool,
}

/// A pending [`Router::treeref`] registration.
pub struct TreeRefRoute<'r, S> {
    pub(super) router: &'r mut Router<S>,
    pub(super) template: String,
}

impl<'r, S> DirRoute<'r, S> {
    /// Register the directory handler and claim the template as a leaf.
    pub fn handler<Marker, H: IntoDirHandler<S, Marker>>(self, h: H) -> Result<&'r mut Router<S>> {
        self.router.dir_at(&self.template, h)?;
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
        self.router.file_at(&self.template, self.ranged, h)?;
        Ok(self.router)
    }
}

impl<'r, S> TreeRefRoute<'r, S> {
    /// Register the subtree-handoff handler and claim the template as a leaf.
    pub fn handler<Marker, H: IntoTreeRefHandler<S, Marker>>(
        self,
        h: H,
    ) -> Result<&'r mut Router<S>> {
        self.router.treeref_at(&self.template, h)?;
        Ok(self.router)
    }
}
