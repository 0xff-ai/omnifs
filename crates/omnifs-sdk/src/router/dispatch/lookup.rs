//! `lookup_child` dispatch.

use crate::browse::Lookup;
use crate::cx::Cx;
use crate::error::Result;
use crate::handler::{DirCx, DirIntent};
use crate::object::ObjectShape;

use super::super::pattern::{parse_child_segment, parse_provider_path};
use super::super::register::Router;

impl<S> Router<S> {
    pub async fn lookup_child(&self, cx: &Cx<S>, parent: &str, name: &str) -> Result<Lookup> {
        debug_assert!(
            parent.starts_with('/'),
            "lookup_child expects an absolute parent path"
        );
        let parent_abs = parse_provider_path(parent)?;
        let child = parse_child_segment(name)?;
        let name = child.as_str();
        let child_abs = parent_abs.join_segment(&child);
        let shape = self.shape();

        if let Some(route) = shape.treeref_route(&child_abs) {
            let tree_ref = (route.entry.handler)(cx.clone(), route.captures).await?;
            return Ok(Lookup::subtree(tree_ref.tree_ref));
        }

        let file_match = shape.file_route(&child_abs);
        if let Some(dir_route) = shape.direct_dir_route(&child_abs) {
            let file_wins = file_match.as_ref().is_some_and(|file_route| {
                file_route.entry.pattern.precedence_key() > dir_route.entry.pattern.precedence_key()
            });
            if !file_wins {
                return Ok(shape.static_dir_lookup(&parent_abs, name));
            }
        }

        if let Some(route) = shape.object_route(&child_abs) {
            return match route.entry.shape {
                ObjectShape::Dir => Ok(shape.static_dir_lookup(&parent_abs, name)),
                ObjectShape::File => Ok(route.entry.child_file_lookup(&parent_abs, name)),
            };
        }

        let object_file_lookup = shape.object_leaf_lookup(&parent_abs, name);
        if object_file_lookup.is_found() {
            return Ok(object_file_lookup);
        }

        if file_match.is_some() {
            return Ok(shape.static_file_lookup(&parent_abs, name));
        }

        if shape.is_implicit_prefix_dir(&child_abs) {
            return Ok(shape.static_dir_lookup(&parent_abs, name));
        }

        if let Some(route) = shape.list_dir_route(&parent_abs) {
            let dir_cx = DirCx::new(
                cx.clone(),
                DirIntent::Lookup {
                    child: name.to_string(),
                },
            );
            let projection = (route.entry.handler)(dir_cx, route.captures).await?;
            return shape.projection_lookup(&parent_abs, name, &projection);
        }

        Ok(Lookup::not_found())
    }
}
