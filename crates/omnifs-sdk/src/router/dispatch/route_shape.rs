//! Route-table shape used by lookup, listing, and read dispatch.

use crate::browse::{Entry as BrowseEntry, List, Listing, Lookup};
use crate::captures::Captures;
use crate::error::Result;
use crate::file_attrs::FileProj;
use crate::object::ObjectShape;
use crate::projection::{DirOutcome, DirProjection};
use omnifs_core::ContentType;
use omnifs_core::path::Path;

use super::super::handlers::{DirEntry, FileEntry, TreeRefEntry};
use super::super::object::{ObjectEntry, ObjectReadTarget};
use super::super::pattern::best_match;
use super::super::projection::merge_entries;
use super::super::register::Router;

pub(in crate::router) struct Shape<'a, S> {
    pub(super) router: &'a Router<S>,
}

pub(in crate::router) struct RouteMatch<'a, E> {
    pub(super) entry: &'a E,
    pub(super) captures: Captures,
}

impl<'a, E> RouteMatch<'a, E> {
    fn new((entry, captures): (&'a E, Captures)) -> Self {
        Self { entry, captures }
    }
}

pub(super) enum ReadRoute<'a, S> {
    File(RouteMatch<'a, FileEntry<S>>),
    Object {
        route: RouteMatch<'a, ObjectEntry<S>>,
        target: ObjectReadTarget,
    },
}

impl<S> Router<S> {
    pub(in crate::router) fn shape(&self) -> Shape<'_, S> {
        Shape { router: self }
    }
}

impl<S> Shape<'_, S> {
    pub(super) fn treeref_route(&self, abs: &Path) -> Option<RouteMatch<'_, TreeRefEntry<S>>> {
        route_match(self.router.treerefs.iter(), abs)
    }

    pub(super) fn direct_dir_route(&self, abs: &Path) -> Option<RouteMatch<'_, DirEntry<S>>> {
        route_match(self.router.dirs.iter(), abs)
    }

    pub(in crate::router) fn list_dir_route(
        &self,
        abs: &Path,
    ) -> Option<RouteMatch<'_, DirEntry<S>>> {
        route_match(
            self.router
                .dirs
                .iter()
                .chain(self.router.handler_dirs.iter()),
            abs,
        )
    }

    pub(in crate::router) fn file_route(&self, abs: &Path) -> Option<RouteMatch<'_, FileEntry<S>>> {
        route_match(
            self.router
                .files
                .iter()
                .chain(self.router.handler_files.iter()),
            abs,
        )
    }

    pub(super) fn object_route(&self, abs: &Path) -> Option<RouteMatch<'_, ObjectEntry<S>>> {
        route_match(self.router.objects.iter(), abs)
    }

    pub(super) fn read_route(&self, abs: &Path, content_type: &str) -> Option<ReadRoute<'_, S>> {
        if let Some(route) = self.file_route(abs) {
            return Some(ReadRoute::File(route));
        }

        if let Some(route) = self.object_route(abs) {
            let ct = ContentType::from_mime(content_type).unwrap_or(ContentType::Octet);
            return Some(ReadRoute::Object {
                route,
                target: ObjectReadTarget::Representation(ct),
            });
        }

        let (parent_abs, leaf) = abs.parent_and_name()?;
        let route = self.object_route(&parent_abs)?;
        if route.entry.shape != ObjectShape::Dir {
            return None;
        }

        if let Some(ct) = route.entry.representation_ct_for_leaf(leaf) {
            return Some(ReadRoute::Object {
                route,
                target: ObjectReadTarget::Representation(ct),
            });
        }
        route.entry.has_file_leaf(leaf).then(|| ReadRoute::Object {
            route,
            target: ObjectReadTarget::Projected(leaf.to_string()),
        })
    }

    pub(super) fn static_dir_lookup(&self, parent_abs: &Path, name: &str) -> Lookup {
        let mut siblings = self.static_entries_for_parent(parent_abs);
        siblings.retain(|entry| entry.name() != name);
        let exhaustive = !self.has_capture_child_under(parent_abs);
        Lookup::entry(BrowseEntry::dir(name))
            .with_siblings(siblings)
            .exhaustive(exhaustive)
    }

    pub(super) fn static_file_lookup(&self, parent_abs: &Path, name: &str) -> Lookup {
        let mut siblings = self.static_entries_for_parent(parent_abs);
        siblings.retain(|entry| entry.name() != name);
        let exhaustive = !self.has_capture_child_under(parent_abs);
        Lookup::entry(BrowseEntry::file(name, FileProj::listing_shape()))
            .with_siblings(siblings)
            .exhaustive(exhaustive)
    }

    pub(super) fn object_leaf_lookup(&self, parent_abs: &Path, name: &str) -> Lookup {
        let Some(route) = self.object_route(parent_abs) else {
            return Lookup::not_found();
        };
        route.entry.child_file_lookup(parent_abs, name)
    }

    pub(super) fn implicit_dir_listing(&self, abs: &Path) -> Option<Listing> {
        self.is_implicit_prefix_dir(abs).then(|| {
            let entries = self.static_entries_for_parent(abs);
            if self.has_capture_child_under(abs) {
                Listing::partial(entries)
            } else {
                Listing::complete(entries)
            }
        })
    }

    pub(super) fn projection_lookup(
        &self,
        parent_abs: &Path,
        name: &str,
        projection: &DirProjection,
    ) -> Result<Lookup> {
        if let DirOutcome::Entries { entries, .. } = projection.outcome() {
            let static_entries = self.static_entries_for_parent(parent_abs);
            let merged = merge_entries(
                entries.iter().map(crate::projection::Entry::name),
                |n| {
                    entries
                        .iter()
                        .find(|entry| entry.name() == n)
                        .map(crate::projection::Entry::to_browse_entry)
                },
                static_entries,
            );
            let target = merged.iter().find(|entry| entry.name() == name).cloned();
            let exhaustive = matches!(
                projection.outcome(),
                DirOutcome::Entries {
                    exhaustive: true,
                    ..
                }
            );
            let siblings = merged.into_iter().filter(|entry| entry.name() != name);
            let lookup = target.map_or_else(Lookup::not_found, Lookup::entry);
            return Ok(lookup
                .with_siblings(siblings)
                .with_effects(projection.project_effects()?)
                .exhaustive(exhaustive));
        }
        Ok(Lookup::not_found())
    }

    pub(super) fn dir_projection_into_list(
        &self,
        abs: &Path,
        projection: &DirProjection,
    ) -> Result<List> {
        match projection.outcome() {
            DirOutcome::Unchanged => Ok(List::unchanged()),
            DirOutcome::Entries {
                exhaustive,
                cursor,
                entries,
            } => {
                let static_entries = self.static_entries_for_parent(abs);
                let merged = merge_entries(
                    entries.iter().map(crate::projection::Entry::name),
                    |name| {
                        entries
                            .iter()
                            .find(|entry| entry.name() == name)
                            .map(crate::projection::Entry::to_browse_entry)
                    },
                    static_entries,
                );

                let mut listing = if *exhaustive {
                    Listing::complete(merged)
                } else {
                    Listing::partial(merged)
                };
                listing = listing.with_effects(projection.project_effects()?);
                if let Some(validator) = projection.validator() {
                    listing = listing.with_validator(validator.0.clone());
                }
                if let Some(cursor) = cursor.clone() {
                    listing = listing.with_cursor(cursor);
                }
                Ok(List::entries(listing))
            },
        }
    }

    pub(super) fn object_dir_listing(&self, entry: &ObjectEntry<S>, anchor_abs: &Path) -> Listing {
        let static_entries = self.static_entries_for_parent(anchor_abs);
        let object_entries = entry.leaves.iter().map(|leaf| {
            if leaf.is_dir {
                BrowseEntry::dir(&leaf.name)
            } else {
                BrowseEntry::file(&leaf.name, FileProj::listing_shape())
            }
        });
        let mut entries = static_entries
            .into_iter()
            .map(|entry| (entry.name().to_string(), entry))
            .collect::<std::collections::BTreeMap<_, _>>();
        for entry in object_entries {
            entries.insert(entry.name().to_string(), entry);
        }
        Listing::complete(entries.into_values())
    }
}

impl<S> ObjectEntry<S> {
    pub(super) fn child_file_lookup(&self, parent_abs: &Path, name: &str) -> Lookup {
        if self.shape == ObjectShape::File {
            if parent_abs.is_root() {
                return Lookup::not_found();
            }
            let parent_name = parent_abs.name();
            if self.leaves.iter().any(|leaf| leaf.name == parent_name) {
                return Lookup::entry(BrowseEntry::file(name, FileProj::listing_shape()));
            }
            return Lookup::not_found();
        }
        if self.has_file_leaf(name) {
            return Lookup::entry(BrowseEntry::file(name, FileProj::listing_shape()));
        }
        Lookup::not_found()
    }

    fn has_file_leaf(&self, name: &str) -> bool {
        self.leaves
            .iter()
            .any(|leaf| leaf.name == name && !leaf.is_dir)
    }

    fn representation_ct_for_leaf(&self, leaf: &str) -> Option<ContentType> {
        let source = format!("{}.{}", self.source_stem, self.source_ext);
        if leaf == source {
            return Some(self.render_table.source_ct);
        }
        for (ct, _) in &self.render_table.renders {
            let ext = ct.extension().unwrap_or("raw");
            if leaf == format!("{}.{}", self.source_stem, ext) {
                return Some(*ct);
            }
        }
        None
    }
}

fn route_match<'a, E, I>(routes: I, abs: &Path) -> Option<RouteMatch<'a, E>>
where
    E: super::super::pattern::RoutedEntry + 'a,
    I: IntoIterator<Item = &'a E>,
{
    best_match(routes, abs).map(RouteMatch::new)
}
