//! `list_children` dispatch.

use crate::browse::List;
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::handler::{Cursor, DirCx, DirIntent};
use crate::object::ObjectShape;

use super::super::pattern::parse_provider_path;
use super::super::register::Router;

impl<S> Router<S> {
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

        if let Some(route) = shape.list_dir_route(&abs) {
            let dir_cx = DirCx::new(cx.clone(), DirIntent::List { cursor });
            let projection = (route.entry.handler)(dir_cx, route.captures).await?;
            return shape.dir_projection_into_list(&abs, &projection);
        }

        if let Some(route) = shape.object_route(&abs) {
            if route.entry.shape == ObjectShape::Dir {
                let effects = (route.entry.list)(cx, route.captures, path.to_string()).await?;
                let listing = shape
                    .object_dir_listing(route.entry, &abs)
                    .with_effects(effects);
                return Ok(List::entries(listing));
            }
            return Err(ProviderError::not_a_directory(format!("{path} is a file")));
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
