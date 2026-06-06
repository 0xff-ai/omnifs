//! Literal-prefix auto-navigation from the route table.

use crate::browse::{Entry as BrowseEntry, EntryKind as BrowseEntryKind};
use crate::captures::Captures;
use crate::file_attrs::FileProj;
use crate::object::ObjectShape;
use crate::router::handlers::RouteValidator;
use omnifs_core::path::{Path, Pattern};

use super::super::pattern::best_match;
use super::route_shape::Shape;

impl<S> Shape<'_, S> {
    pub(in crate::router) fn static_entries_for_parent(
        &self,
        absolute_parent: &Path,
    ) -> Vec<BrowseEntry> {
        let parent_segments: Vec<&str> = absolute_parent.segments().collect();
        let mut entries = std::collections::BTreeMap::<String, BrowseEntry>::new();
        for (pattern, validator, kind) in self.routes_extending_parent(&parent_segments) {
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
                } else {
                    BrowseEntry::file(name, FileProj::listing_shape())
                }
            });
        }
        entries.into_values().collect()
    }

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
        let Some((parent_abs, name)) = absolute_path.parent_and_name() else {
            return false;
        };
        self.static_entries_for_parent(&parent_abs)
            .iter()
            .any(|entry| entry.name() == name && entry.kind() == BrowseEntryKind::Directory)
    }

    pub(super) fn has_capture_child_under(&self, absolute_parent: &Path) -> bool {
        let parent_segments: Vec<&str> = absolute_parent.segments().collect();
        self.routes_extending_parent(&parent_segments)
            .any(|(pattern, _, _)| pattern.has_dynamic_child_after(&parent_segments))
    }

    fn routes_extending_parent<'a>(
        &'a self,
        parent_segments: &'a [&'a str],
    ) -> impl Iterator<Item = (&'a Pattern, &'a RouteValidator, BrowseEntryKind)> + 'a {
        let dirs = self
            .router
            .dirs
            .iter()
            .chain(self.router.handler_dirs.iter())
            .map(|r| (&r.pattern, &r.validator, BrowseEntryKind::Directory));
        let files = self
            .router
            .files
            .iter()
            .chain(self.router.handler_files.iter())
            .map(|r| (&r.pattern, &r.validator, BrowseEntryKind::File));
        let treerefs = self
            .router
            .treerefs
            .iter()
            .map(|r| (&r.pattern, &r.validator, BrowseEntryKind::Directory));
        let objects = self.router.objects.iter().map(|r| {
            let kind = match r.shape {
                ObjectShape::Dir => BrowseEntryKind::Directory,
                ObjectShape::File => BrowseEntryKind::File,
            };
            (&r.pattern, &r.validator, kind)
        });
        dirs.chain(files)
            .chain(treerefs)
            .chain(objects)
            .filter(move |(pattern, _, _)| pattern.accepts_as_strict_ancestor(parent_segments))
    }
}
