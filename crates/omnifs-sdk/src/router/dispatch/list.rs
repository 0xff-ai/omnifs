//! `list_children` dispatch.

use crate::browse::List;
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::handler::{Cursor, DirCx, DirIntent};

use super::super::pattern::parse_provider_path;
use super::super::register::Router;

impl<S> Router<S> {
    /// List an absolute directory path.
    ///
    /// Resolution order: treeref handoff; a dir route run with
    /// [`DirIntent::List`] and the resume cursor; an object anchor
    /// (precomputed leaf names, with the load's canonical-store and
    /// eager-preload effects attached); an auto-navigable literal prefix
    /// listed from the route table alone. File routes report not-a-directory.
    ///
    /// Handler and anchor listings are merged with the literal sibling
    /// routes registered at that depth, ordered by name, the handler winning
    /// name collisions. Exhaustiveness: a handler listing reports the
    /// handler's own flag; an object anchor listing is complete (its leaves
    /// are statically known); an implicit prefix listing is partial whenever
    /// a capture sibling at the next depth can bind names that cannot be
    /// enumerated. Lookup remains the authority for names a listing
    /// omitted.
    ///
    /// `cached_validator` is accepted for the WIT contract but unused here:
    /// the host delivers the listing validator through the context (see
    /// [`Cx::version`]), and handlers opt in by returning
    /// [`crate::projection::DirProjection::unchanged`] when it still holds.
    pub async fn list_children(
        &self,
        cx: &Cx<S>,
        path: &str,
        cached_validator: Option<String>,
        cursor: Option<Cursor>,
    ) -> Result<List> {
        debug_assert!(
            path.starts_with('/'),
            "list_children expects an absolute path"
        );
        let _ = cached_validator;
        let abs = parse_provider_path(path)?;
        let shape = self.shape();

        if let Some(route) = shape.treeref_route(&abs) {
            let tree_ref = (route.entry.handler)(cx.clone(), route.captures).await?;
            return Ok(List::subtree(tree_ref.tree_ref));
        }

        if let Some(route) = shape.dir_route(&abs) {
            let dir_cx = DirCx::new(cx.clone(), DirIntent::List { cursor });
            let projection = (route.entry.handler)(dir_cx, route.captures).await?;
            return shape.dir_projection_into_list(&abs, &projection);
        }

        if let Some(route) = shape.object_route(&abs) {
            let out = (route.entry.list)(cx, route.captures.clone(), path.to_string()).await?;
            let mut listing = shape
                .object_dir_listing(route.entry, &abs, out.source.as_ref())
                .with_effects(out.effects);
            if route.entry.has_anchor_collections() {
                let projections = route
                    .entry
                    .run_anchor_collections(cx, &route.captures)
                    .await?;
                listing = super::route_shape::merge_anchor_collections(&listing, &projections)?;
            }
            return Ok(List::entries(listing));
        }

        if let Some(listing) = shape.implicit_dir_listing(&abs) {
            return Ok(List::entries(listing));
        }

        if shape.file_route(&abs).is_some() {
            return Err(ProviderError::not_a_directory(format!("{path} is a file")));
        }

        Err(ProviderError::not_found(format!("path not found: {path}")))
    }
}
