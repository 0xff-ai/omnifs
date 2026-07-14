//! `list_children` dispatch.

use crate::browse::List;
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::handler::{Cursor, DirCx, DirIntent};
use omnifs_core::path::Path;

use super::super::compiled::CompiledRouter;

impl<S> CompiledRouter<S> {
    /// List an absolute directory path.
    ///
    /// Resolution order: treeref handoff; a dir route run with
    /// [`DirIntent::List`] and the resume cursor; an object anchor
    /// (precomputed leaf names plus its optional cursor-bearing collection,
    /// with canonical-store and eager-preload effects attached); an
    /// auto-navigable literal prefix listed from the route table alone. File
    /// routes report not-a-directory.
    ///
    /// Handler and anchor listings are merged with the literal sibling
    /// routes registered at that depth, ordered by name, the handler winning
    /// name collisions. Exhaustiveness: a handler listing reports the
    /// handler's own flag; a bare object anchor listing is complete because
    /// its leaves are statically known, while an attached collection carries
    /// its own completeness; an implicit prefix listing is partial whenever a
    /// capture sibling at the next depth can bind names that cannot be
    /// enumerated. Lookup remains the authority for names a listing omitted.
    ///
    /// `cached_validator` is accepted for the WIT contract but unused here:
    /// the host delivers the listing validator through the context (see
    /// [`Cx::version`]), and handlers opt in by returning
    /// [`crate::projection::DirProjection::unchanged`] when it still holds.
    pub async fn list_children(
        &self,
        cx: &Cx<S>,
        path: &str,
        _cached_validator: Option<String>,
        cursor: Option<Cursor>,
    ) -> Result<List> {
        debug_assert!(
            path.starts_with('/'),
            "list_children expects an absolute path"
        );
        let abs =
            Path::parse(path).map_err(|error| ProviderError::invalid_input(error.to_string()))?;
        let shape = self.shape();

        if let Some(route) = shape.treeref_route(&abs) {
            let tree_ref = super::route_future(
                route.entry.pattern.template(),
                (route.entry.handler)(cx.clone(), route.captures),
            )
            .await
            .map_err(|error| error.with_context("list-children", &abs))?;
            return Ok(List::subtree(tree_ref.tree_ref));
        }

        if let Some(route) = shape.dir_route(&abs) {
            let dir_cx = DirCx::new(cx.clone(), DirIntent::List { cursor });
            let listing = super::route_future(
                route.entry.pattern.template(),
                (route.entry.handler)(dir_cx, route.captures),
            )
            .await
            .map_err(|error| error.with_context("list-children", &abs))?;
            return shape.dir_projection_into_list(&abs, &listing.into_dir_projection());
        }

        if let Some(route) = shape.object_route(&abs) {
            let out = super::route_future(
                route.entry.pattern.template(),
                (route.entry.list)(cx, route.captures.clone(), path.to_string()),
            )
            .await
            .map_err(|error| error.with_context("list-children", &abs))?;
            let mut listing = shape
                .object_dir_listing(route.entry, &abs, out.source.as_ref())
                .with_effects(out.effects);
            if let Some(projection) = super::route_future(
                route.entry.pattern.template(),
                Box::pin(
                    route
                        .entry
                        .run_anchor_collection(cx, &route.captures, cursor),
                ),
            )
            .await
            .map_err(|error| error.with_context("list-children", &abs))?
            {
                listing = super::route_shape::merge_anchor_collection(&listing, &projection)?;
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
