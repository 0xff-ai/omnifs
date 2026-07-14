//! Executable provider route table produced by
//! [`Router::compile`](super::Router::compile).

use super::descriptor::RouteDescriptor;
use super::handlers::{DirEntry, FileEntry, TreeRefEntry};
use super::object::ObjectRouteEntry;

/// An immutable, executable provider route table produced by
/// [`Router::compile`](super::Router::compile).
///
/// Compilation resolves and validates all registration-only state before
/// constructing this type. It therefore contains only the tables used by
/// runtime dispatch and the stable descriptors exposed to provider tooling.
pub struct CompiledRouter<S = ()> {
    pub(super) dirs: Vec<DirEntry<S>>,
    pub(super) files: Vec<FileEntry<S>>,
    pub(super) treerefs: Vec<TreeRefEntry<S>>,
    pub(super) objects: Vec<ObjectRouteEntry<S>>,
    route_descriptors: Vec<RouteDescriptor>,
}

impl<S> CompiledRouter<S> {
    pub(super) fn new(
        dirs: Vec<DirEntry<S>>,
        files: Vec<FileEntry<S>>,
        treerefs: Vec<TreeRefEntry<S>>,
        objects: Vec<ObjectRouteEntry<S>>,
        route_descriptors: Vec<RouteDescriptor>,
    ) -> Self {
        Self {
            dirs,
            files,
            treerefs,
            objects,
            route_descriptors,
        }
    }

    /// Return the stable descriptors captured when the router was compiled.
    #[must_use]
    pub fn routes(&self) -> &[RouteDescriptor] {
        &self.route_descriptors
    }
}
