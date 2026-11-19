//! `list_children` dispatch.

use crate::browse::{List, Listing};
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::handler::{Cursor, DirIntent};
use crate::projection::DirPage;
use omnifs_core::path::Path;

use super::super::compiled::CompiledRouter;

impl<S> CompiledRouter<S> {
    /// List an absolute directory path.
    ///
    /// Resolution order: a tree-capable directory handoff or a dir route run
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
    /// The validator of the cached page of `cursor` ([`Cx::version`]) reaches
    /// a dir route and the collection of an object anchor, the owners of the
    /// page, and never the anchor object's own load. A call with the
    /// validator of a page, or for a later page, lists an object anchor
    /// without the load of its object, which the listing of its first page
    /// loads: an unchanged page answers `unchanged`, a changed first page has
    /// the leaves of the anchor without the source attributes of its
    /// canonical leaf, and a later page has none of them.
    pub async fn list_children(
        &self,
        cx: &Cx<S>,
        path: &str,
        cursor: Option<Cursor>,
    ) -> Result<List> {
        debug_assert!(
            path.starts_with('/'),
            "list_children expects an absolute path"
        );
        let abs =
            Path::parse(path).map_err(|error| ProviderError::invalid_input(error.to_string()))?;
        let shape = self.shape();

        if let Some(route) = shape.dir_route(&abs) {
            let answer = super::route_future(
                route.entry.pattern.template(),
                Box::pin(route.entry.invoke(
                    cx.clone(),
                    DirIntent::List { cursor },
                    route.captures,
                )),
            )
            .await
            .map_err(|error| error.with_context("list-children", &abs))?;
            return match answer {
                super::super::handlers::DirAnswer::Tree(tree_ref) => {
                    Ok(List::subtree(tree_ref.tree_ref))
                },
                super::super::handlers::DirAnswer::Listing(listing) => {
                    shape.dir_projection_into_list(&abs, &listing.into_dir_projection())
                },
            };
        }

        if let Some(route) = shape.object_route(&abs) {
            let collection = async |cursor| {
                super::route_future(
                    route.entry.pattern.template(),
                    Box::pin(
                        route
                            .entry
                            .run_anchor_collection(cx, &route.captures, cursor),
                    ),
                )
                .await
                .map_err(|error| error.with_context("list-children", &abs))
            };
            if cx.version().is_some() || cursor.is_some() {
                let first = cursor.is_none();
                let projection = collection(cursor).await?;
                if let Some(projection) = &projection
                    && let DirPage::Unchanged { validator } = projection.page()
                {
                    return Ok(List::unchanged(
                        validator.clone(),
                        projection.project_effects()?,
                    ));
                }
                let listing = if first {
                    shape.object_dir_listing(route.entry, &abs, None)
                } else {
                    Listing::complete(Vec::new())
                };
                let Some(projection) = projection else {
                    return Ok(List::entries(listing));
                };
                return super::route_shape::merge_anchor_collection(&listing, &projection);
            }
            let out = super::route_future(
                route.entry.pattern.template(),
                (route.entry.list)(cx, route.captures.clone(), path.to_string()),
            )
            .await
            .map_err(|error| error.with_context("list-children", &abs))?;
            let listing = shape
                .object_dir_listing(route.entry, &abs, out.source.as_ref())
                .with_effects(out.effects);
            let Some(projection) = collection(cursor).await? else {
                return Ok(List::entries(listing));
            };
            return super::route_shape::merge_anchor_collection(&listing, &projection);
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
