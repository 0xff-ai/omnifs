//! `lookup_child` dispatch.

use crate::browse::Lookup;
use crate::cx::Cx;
use crate::error::{ProviderError, Result};
use crate::handler::{DirCx, DirIntent};
use omnifs_core::path::{Path, Segment};

use super::super::compiled::CompiledRouter;

impl<S> CompiledRouter<S> {
    /// Resolve one child name under an absolute parent path.
    ///
    /// Lookup is the authoritative name oracle: a found result here is
    /// binding even when the name never appeared in a listing, which is what
    /// lets capture routes resolve arbitrary names (`cd /github/torvalds`)
    /// without enumeration. Resolution order:
    ///
    /// 1. A treeref route at the child path: run the handler and hand the
    ///    subtree to the host.
    /// 2. A dir route at the child path: answer statically, without running
    ///    the child's handler, unless a file route also matches with
    ///    strictly higher precedence (then the file claims the name).
    /// 3. An object anchored at the child path: answer as a static directory.
    /// 4. A representation or field leaf of an object anchored at the
    ///    parent.
    /// 5. A file route at the child path: answer statically.
    /// 6. An auto-navigable literal prefix: a directory synthesized from the
    ///    route table alone.
    /// 7. Fallback: run the parent's dir handler with
    ///    [`DirIntent::Lookup`] and resolve the name against its enumeration
    ///    merged with static siblings; this is how dynamically enumerated
    ///    children (which match no route of their own) resolve.
    ///
    /// Static answers carry their sibling entries and an `exhaustive` flag
    /// that is false whenever a capture sibling exists at this depth, so the
    /// host knows the names it cached are not the whole story.
    pub async fn lookup_child(&self, cx: &Cx<S>, parent: &str, name: &str) -> Result<Lookup> {
        debug_assert!(
            parent.starts_with('/'),
            "lookup_child expects an absolute parent path"
        );
        let parent_abs =
            Path::parse(parent).map_err(|error| ProviderError::invalid_input(error.to_string()))?;
        let child = Segment::try_from(name)
            .map_err(|error| ProviderError::invalid_input(error.to_string()))?;
        let name = child.as_str();
        let child_abs = parent_abs.join_segment(&child);
        let shape = self.shape();

        if let Some(route) = shape.treeref_route(&child_abs) {
            let tree_ref = super::route_future(
                route.entry.pattern.template(),
                route.entry.handler.call(cx, (), route.captures),
            )
            .await
            .map_err(|error| error.with_context("lookup-child", &child_abs))?;
            return Ok(Lookup::subtree(tree_ref.tree_ref));
        }

        let file_match = shape.file_route(&child_abs);
        if let Some(dir_route) = shape.dir_route(&child_abs) {
            let file_wins = file_match.as_ref().is_some_and(|file_route| {
                file_route.entry.pattern.precedence_key() > dir_route.entry.pattern.precedence_key()
            });
            if !file_wins {
                return Ok(shape.static_dir_lookup(&parent_abs, name));
            }
        }

        if let Some(object_route) = shape.object_route(&child_abs) {
            let file_wins = file_match.as_ref().is_some_and(|file_route| {
                file_route.entry.pattern.precedence_key()
                    > object_route.entry.pattern.precedence_key()
            });
            if !file_wins {
                return Ok(shape.static_dir_lookup(&parent_abs, name));
            }
        }

        let object_file_lookup = shape.object_leaf_lookup(&parent_abs, name);
        if object_file_lookup.is_found() {
            return Ok(object_file_lookup);
        }

        if let Some(file_route) = file_match.as_ref() {
            return Ok(shape.static_file_lookup(&parent_abs, name, file_route.entry.ranged));
        }

        if shape.is_implicit_prefix_dir(&child_abs) {
            return Ok(shape.static_dir_lookup(&parent_abs, name));
        }

        if let Some(route) = shape.dir_route(&parent_abs) {
            let dir_cx = DirCx::new(
                cx.clone(),
                DirIntent::Lookup {
                    child: name.to_string(),
                },
            );
            let listing = super::route_future(
                route.entry.pattern.template(),
                route.entry.handler.call(&dir_cx, (), route.captures),
            )
            .await
            .map_err(|error| error.with_context("lookup-child", &child_abs))?;
            return shape.projection_lookup(&parent_abs, name, &listing.into_dir_projection());
        }

        Ok(Lookup::not_found())
    }
}
