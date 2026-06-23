//! Route-table shape used by lookup, listing, and read dispatch.
//!
//! [`Shape`] is a borrowed view over the sealed router that centralizes
//! route selection (per-kind `best_match` queries) and result assembly
//! (static lookups, projection-to-listing lowering, entry merging), so the
//! three entry points share one set of rules.

use crate::browse::{Entry as BrowseEntry, List, Listing, Lookup};
use crate::captures::Captures;
use crate::error::Result;
use crate::file_attrs::{FileProj, ReadMode, Size};
use crate::object::ObjectShape;
use crate::projection::{DirOutcome, DirProjection};
use omnifs_core::ContentType;
use omnifs_core::path::Path;

use super::super::handlers::{DirEntry, FileEntry, TreeRefEntry};
use super::super::object::{ObjectEntry, ObjectReadTarget, SourceLeafAttrs};
use super::super::pattern::best_match;
use super::super::projection::merge_entries;
use super::super::register::Router;

/// A borrowed dispatch view over the sealed route tables.
pub(in crate::router) struct Shape<'a, S> {
    pub(super) router: &'a Router<S>,
}

/// A selected route plus the captures its pattern decoded from the path;
/// the validator has already accepted them.
pub(in crate::router) struct RouteMatch<'a, E> {
    pub(super) entry: &'a E,
    pub(super) captures: Captures,
}

impl<'a, E> RouteMatch<'a, E> {
    fn new((entry, captures): (&'a E, Captures)) -> Self {
        Self { entry, captures }
    }
}

/// How a read path resolves: through a file handler, or through the object
/// read path with a resolved target.
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

    /// Dir routes registered via `r.dir(..)` only; object handler dirs are
    /// excluded. Lookup uses this for its file-vs-dir precedence comparison.
    pub(super) fn direct_dir_route(&self, abs: &Path) -> Option<RouteMatch<'_, DirEntry<S>>> {
        route_match(self.router.dirs.iter(), abs)
    }

    /// Dir routes including object handler dirs: the set whose handlers can
    /// answer a listing or a lookup fallback.
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

    /// File routes including object handler files.
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

    /// Resolve a read path: file route first; then an object anchor at the
    /// path (representation chosen by the requested content type); then a
    /// leaf one level under a dir-shaped anchor, where the leaf name
    /// resolves to a representation (`stem.ext` against the render table)
    /// before a projected field.
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

    /// A directory answer synthesized from the route table: no handler runs.
    /// Carries the parent's other static entries as siblings (one host
    /// round trip warms the whole directory) and is exhaustive only when no
    /// capture sibling can bind further names at this depth.
    pub(super) fn static_dir_lookup(&self, parent_abs: &Path, name: &str) -> Lookup {
        let mut siblings = self.static_entries_for_parent(parent_abs);
        siblings.retain(|entry| entry.name() != name);
        let exhaustive = !self.has_capture_child_under(parent_abs);
        Lookup::entry(BrowseEntry::dir(name))
            .with_siblings(siblings)
            .exhaustive(exhaustive)
    }

    /// The file analog of [`Self::static_dir_lookup`]; the entry carries the
    /// listing-shape projection (size and bytes resolve at read time). A route
    /// declared `ranged` projects `ReadMode::Ranged` so the host dispatches
    /// `open` straight to `open-file`.
    pub(super) fn static_file_lookup(&self, parent_abs: &Path, name: &str, ranged: bool) -> Lookup {
        let mut siblings = self.static_entries_for_parent(parent_abs);
        siblings.retain(|entry| entry.name() != name);
        let exhaustive = !self.has_capture_child_under(parent_abs);
        let shape = if ranged {
            FileProj::ranged_listing_shape()
        } else {
            FileProj::listing_shape()
        };
        Lookup::entry(BrowseEntry::file(name, shape))
            .with_siblings(siblings)
            .exhaustive(exhaustive)
    }

    /// Resolve `name` as a leaf of an object anchored at `parent_abs`;
    /// not-found when no object is anchored there.
    pub(super) fn object_leaf_lookup(&self, parent_abs: &Path, name: &str) -> Lookup {
        let Some(route) = self.object_route(parent_abs) else {
            return Lookup::not_found();
        };
        route.entry.child_file_lookup(parent_abs, name)
    }

    /// List an auto-navigable literal prefix from the route table alone:
    /// partial when a capture sibling at the next depth can bind names this
    /// enumeration cannot produce, complete otherwise.
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

    /// Resolve a lookup through the parent handler's enumeration: merge the
    /// projection's entries with the static siblings (projection wins name
    /// collisions), pick the target by name, and carry the rest as
    /// siblings together with the projection's preload effects, so a single
    /// lookup warms everything the handler already computed. The
    /// `Unchanged` outcome resolves to not-found here: it enumerates
    /// nothing to match against.
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

    /// Lower a [`DirProjection`] to the wire listing: merge with static
    /// siblings, honor the handler's exhaustive flag, and attach the
    /// preload effects, re-list validator, and resume cursor the projection
    /// carries.
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

    /// An object anchor's listing: the precomputed leaf names merged over
    /// the static entries (leaves win collisions), always complete because
    /// an anchor's children are statically declared.
    ///
    /// `source` carries the loaded canonical's exact attrs; the verbatim source
    /// representation leaf is stamped with them so a cold `ls -l` reports its
    /// real size. Rendered leaves stay size-unknown (their length needs a
    /// render) and lookups remain placeholder until a listing or read fills in.
    pub(super) fn object_dir_listing(
        &self,
        entry: &ObjectEntry<S>,
        anchor_abs: &Path,
        source: Option<&SourceLeafAttrs>,
    ) -> Listing {
        let static_entries = self.static_entries_for_parent(anchor_abs);
        let source_leaf_name = format!("{}.{}", entry.source_stem, entry.source_ext);
        let object_entries = entry.leaves.iter().map(|leaf| {
            if leaf.is_dir {
                BrowseEntry::dir(&leaf.name)
            } else if leaf.name == source_leaf_name
                && let Some(source) = source
            {
                BrowseEntry::file(&leaf.name, source_leaf_shape(source))
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
    /// Resolve a leaf name against this entry's declared file leaves. For a
    /// dir-shaped object, `name` must be one of the anchor's file leaves;
    /// the file-shaped branch instead tests `parent_abs`'s final segment
    /// against the leaf names.
    ///
    /// A dir-shaped match carries the anchor's OTHER leaves as exhaustive
    /// siblings. The leaf set is statically known (it is the same set
    /// [`Shape::object_dir_listing`] enumerates), so a single child lookup
    /// teaches the host the whole directory in one round trip. Omitting the
    /// siblings while still reporting `exhaustive` (the default) would make the
    /// host's lookup-hints merge treat the directory as containing only the
    /// looked-up child, collapsing a later readdir to that one entry.
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
            return Lookup::entry(BrowseEntry::file(name, FileProj::listing_shape()))
                .with_siblings(self.sibling_leaves(name));
        }
        Lookup::not_found()
    }

    /// The anchor's leaves other than `target`, as browse entries: the sibling
    /// set a dir-shaped child lookup carries so the host caches the whole
    /// (statically known, exhaustive) directory from one lookup. Mirrors the
    /// leaf-to-entry mapping in [`Shape::object_dir_listing`]; placeholder
    /// shapes are used because a lookup does not load the canonical, so leaf
    /// sizes (including the source leaf) resolve at read time.
    fn sibling_leaves(&self, target: &str) -> Vec<BrowseEntry> {
        self.leaves
            .iter()
            .filter(|leaf| leaf.name != target)
            .map(|leaf| {
                if leaf.is_dir {
                    BrowseEntry::dir(&leaf.name)
                } else {
                    BrowseEntry::file(&leaf.name, FileProj::listing_shape())
                }
            })
            .collect()
    }

    fn has_file_leaf(&self, name: &str) -> bool {
        self.leaves
            .iter()
            .any(|leaf| leaf.name == name && !leaf.is_dir)
    }

    /// Map a `stem.ext` leaf name back to its representation content type:
    /// the canonical source first, then each registered render by its
    /// extension.
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

/// Listing shape for the verbatim source representation leaf: a full-deferred
/// entry whose exact size and version are already known from the loaded
/// canonical bytes, so a cold `ls -l` reports the real size.
fn source_leaf_shape(source: &SourceLeafAttrs) -> FileProj {
    let shape = FileProj::deferred(Size::Exact(source.len), ReadMode::Full, source.stability);
    match &source.validator {
        Some(validator) => shape.with_version(validator.clone()),
        None => shape,
    }
}

fn route_match<'a, E, I>(routes: I, abs: &Path) -> Option<RouteMatch<'a, E>>
where
    E: super::super::pattern::RoutedEntry + 'a,
    I: IntoIterator<Item = &'a E>,
{
    best_match(routes, abs).map(RouteMatch::new)
}
