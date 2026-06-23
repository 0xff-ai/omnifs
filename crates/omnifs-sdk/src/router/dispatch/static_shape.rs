//! Literal-prefix auto-navigation from the route table.
//!
//! This is what makes intermediate directories free: every literal segment
//! on the way to a registered route resolves and lists without a handler,
//! so providers never register stub routes for navigation scaffolding.
//! Synthesized entries are derived purely from route patterns, filtered by
//! each route's present-capture validator so a parse-rejecting prefix does
//! not advertise children it could never serve.

use super::super::pattern::Pattern;
use crate::browse::{Entry as BrowseEntry, EntryKind as BrowseEntryKind};
use crate::captures::Captures;
use crate::file_attrs::FileProj;
use crate::router::handlers::RouteValidator;
use omnifs_core::path::Path;

use super::super::pattern::best_match;
use super::route_shape::Shape;

impl<S> Shape<'_, S> {
    /// The literal child entries every route contributes directly under a
    /// parent, name-deduplicated and name-ordered.
    ///
    /// A child is a directory when the contributing route continues below it
    /// or is itself dir-kinded; on duplicate names the first contributing
    /// route wins (iteration order: dir routes, file routes, treerefs,
    /// objects). Routes whose already-present captures fail the prefix
    /// validator are skipped. Capture children are invisible here by
    /// construction; that gap is what the exhaustiveness rules account for.
    pub(in crate::router) fn static_entries_for_parent(
        &self,
        absolute_parent: &Path,
    ) -> Vec<BrowseEntry> {
        let parent_segments: Vec<&str> = absolute_parent.segments().collect();
        let mut entries = std::collections::BTreeMap::<String, BrowseEntry>::new();
        for (pattern, validator, kind, ranged) in self.routes_extending_parent(&parent_segments) {
            let Some((name, extends_below)) = pattern.literal_child_after(&parent_segments) else {
                continue;
            };
            let Ok(child_abs) = absolute_parent.join(name) else {
                continue;
            };
            let Some(()) = pattern.match_prefix(&child_abs).ok().and_then(|matched| {
                validator
                    .accepts_present(&Captures::from_match(&matched))
                    .then_some(())
            }) else {
                continue;
            };
            entries.entry(name.to_string()).or_insert_with(|| {
                if extends_below || matches!(kind, BrowseEntryKind::Directory) {
                    BrowseEntry::dir(name)
                } else if ranged {
                    BrowseEntry::file(name, FileProj::ranged_listing_shape())
                } else {
                    BrowseEntry::file(name, FileProj::listing_shape())
                }
            });
        }
        entries.into_values().collect()
    }

    /// Whether a path is an auto-navigable intermediate directory: not
    /// itself a dir/file/treeref/object route (those answer through their
    /// own dispatch arms), but either a concrete path with literal children
    /// beneath it or a literal directory child its parent's static entries
    /// advertise. The root qualifies whenever any route at all is registered.
    pub(super) fn is_implicit_prefix_dir(&self, absolute_path: &Path) -> bool {
        if best_match(self.router.dirs.iter(), absolute_path).is_some()
            || best_match(self.router.files.iter(), absolute_path).is_some()
            || best_match(self.router.treerefs.iter(), absolute_path).is_some()
            || best_match(self.router.objects.iter(), absolute_path).is_some()
        {
            return false;
        }
        if absolute_path.is_root() {
            return !self.router.dirs.is_empty()
                || !self.router.files.is_empty()
                || !self.router.treerefs.is_empty()
                || !self.router.objects.is_empty();
        }
        if !self.static_entries_for_parent(absolute_path).is_empty() {
            return true;
        }
        let Some((parent_abs, name)) = absolute_path.parent_and_name() else {
            return false;
        };
        self.static_entries_for_parent(&parent_abs)
            .iter()
            .any(|entry| entry.name() == name && entry.kind() == BrowseEntryKind::Directory)
    }

    /// Whether any route binds a capture or rest segment directly under the
    /// parent. True forces static lookups and implicit listings at this
    /// depth to report non-exhaustive: names matched by the capture exist
    /// but cannot be enumerated from the route table.
    pub(super) fn has_capture_child_under(&self, absolute_parent: &Path) -> bool {
        let parent_segments: Vec<&str> = absolute_parent.segments().collect();
        self.routes_extending_parent(&parent_segments)
            .any(|(pattern, _, _, _)| pattern.has_dynamic_child_after(&parent_segments))
    }

    /// Every route whose pattern extends strictly below the parent, with the
    /// entry kind its leaf would have.
    fn routes_extending_parent<'a>(
        &'a self,
        parent_segments: &'a [&'a str],
    ) -> impl Iterator<Item = (&'a Pattern, &'a RouteValidator, BrowseEntryKind, bool)> + 'a {
        let dirs = self
            .router
            .dirs
            .iter()
            .map(|r| (&r.pattern, &r.validator, BrowseEntryKind::Directory, false));
        let files = self
            .router
            .files
            .iter()
            .map(|r| (&r.pattern, &r.validator, BrowseEntryKind::File, r.ranged));
        let treerefs = self
            .router
            .treerefs
            .iter()
            .map(|r| (&r.pattern, &r.validator, BrowseEntryKind::Directory, false));
        let objects = self
            .router
            .objects
            .iter()
            .map(|r| (&r.pattern, &r.validator, BrowseEntryKind::Directory, false));
        dirs.chain(files)
            .chain(treerefs)
            .chain(objects)
            .filter(move |(pattern, _, _, _)| pattern.accepts_as_strict_ancestor(parent_segments))
    }
}
