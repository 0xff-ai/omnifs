//! Route registration and object mounting.
//!
//! Everything here runs once, inside a provider's `start`. The route table is
//! append-only and immutable after [`Router::seal`]; dispatch (the
//! `dispatch` submodule) reads it on every host browse call.

use crate::error::{ProviderError, Result};
use crate::object::{FacetMetadata, Key, Object};

use super::handlers::{IntoDirHandler, IntoFileHandler, IntoTreeRefHandler};
use super::object::{AnchorShape, ObjectBlock, ObjectHandle, mount_object, object};
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
    /// Collection faces declared on object dir faces, resolved against the
    /// registry at seal time.
    pub(super) collections: Vec<CollectionRef>,
}

/// Registry record for one mounted object kind, used by collection faces to
/// resolve the child object's anchor template and whether it can be `fresh`.
pub(super) struct RegisteredObject {
    pub kind_str: &'static str,
    pub template: String,
    pub has_canonical: bool,
}

/// A collection face awaiting registry resolution at seal time.
pub(super) struct CollectionRef {
    /// The collection dir path (`parent_anchor/name`).
    pub dir_path: String,
    pub child_kind_str: &'static str,
    /// Whether the collection can emit `fresh` entries (always true for the
    /// typed `collection::<C>` form).
    pub requires_canonical: bool,
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
    pub fn object<O: Object + 'static>(
        &mut self,
        template: &'static str,
        block: impl FnOnce(&mut ObjectBlock<O>) -> Result<()>,
    ) -> Result<ObjectHandle<O>>
    where
        O::Key: Key + FacetMetadata + 'static,
        O::State: 'static,
        S: 'static,
        O: Object<State = S>,
    {
        let handle = object(template, block)?;
        self.mount_handle(template, &handle)?;
        Ok(handle)
    }

    /// Bind a file-shaped [`Object`] at `template`: the anchor projects as a
    /// single file (one canonical/representation/direct/blob face), not a
    /// directory.
    pub fn file_object<O: Object + 'static>(
        &mut self,
        template: &'static str,
        block: impl FnOnce(&mut ObjectBlock<O>) -> Result<()>,
    ) -> Result<ObjectHandle<O>>
    where
        O::Key: Key + FacetMetadata + 'static,
        O::State: 'static,
        S: 'static,
        O: Object<State = S>,
    {
        let handle = super::object::file_object(template, block)?;
        self.mount_handle(template, &handle)?;
        Ok(handle)
    }

    /// Mount the same object spec at another `template` (an alias). Captures
    /// must satisfy the key (checked at seal time).
    pub fn alias<O: Object + 'static>(
        &mut self,
        template: &'static str,
        handle: &ObjectHandle<O>,
    ) -> Result<&mut Self>
    where
        O::Key: Key + FacetMetadata + 'static,
        O::State: 'static,
        S: 'static,
        O: Object<State = S>,
    {
        self.mount_handle(template, handle)?;
        Ok(self)
    }

    fn mount_handle<O: Object + 'static>(
        &mut self,
        template: &'static str,
        handle: &ObjectHandle<O>,
    ) -> Result<()>
    where
        O::Key: Key + FacetMetadata + 'static,
        O::State: 'static,
        S: 'static,
        O: Object<State = S>,
    {
        if !template.starts_with('/') {
            return Err(ProviderError::invalid_input(format!(
                "object template must be absolute: {template}"
            )));
        }
        let pattern = parse_pattern(template)?;
        let mounted = mount_object::<O>(&pattern, handle.spec.as_ref(), template)?;
        self.object_registry.push(RegisteredObject {
            kind_str: O::kind().as_str(),
            template: template.to_string(),
            has_canonical: handle.has_canonical(),
        });
        for decl in handle.collection_decls() {
            self.collections.push(CollectionRef {
                dir_path: decl.dir_path.clone(),
                child_kind_str: decl.child_kind_str,
                requires_canonical: decl.requires_canonical,
            });
        }
        // Register the SDK-generated collection dir handlers as dir routes so a
        // readdir of the collection path runs the typed list method.
        for (dir_path, handler) in handle.collection_handlers() {
            let pattern = parse_pattern(dir_path)?;
            let validator = super::handlers::captures_validator::<O::Key>();
            let handler = handler.clone();
            let boxed: super::handlers::BoxedDirHandler<S> =
                std::sync::Arc::new(move |dir_cx, caps| handler(dir_cx, caps));
            self.dirs.push(super::handlers::DirEntry {
                pattern,
                handler: boxed,
                validator,
            });
            // The dir path is already claimed by the collection face's leaf
            // claim; do not re-claim it here (that would self-overlap).
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

    /// Seal-time validation, called by the `#[omnifs_sdk::provider]` glue after
    /// `start` returns; providers do not call it themselves.
    ///
    /// Runs, in order:
    /// - one-path-one-route overlap (equal-precedence leaf claims that can bind
    ///   the same concrete path);
    /// - every collection face resolves to a registered child object that has a
    ///   canonical face when `fresh` entries are allowed.
    ///
    /// The face-level checks (canonical CT, single canonical, representation
    /// without canonical, Live only on stream, reserved `@`) run at build time
    /// inside the block builder and need no re-check here.
    pub fn seal(&self) -> Result<()> {
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

        for collection in &self.collections {
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
        }

        Ok(())
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

// Keep AnchorShape reachable for documentation links.
#[allow(unused_imports)]
use AnchorShape as _AnchorShape;
