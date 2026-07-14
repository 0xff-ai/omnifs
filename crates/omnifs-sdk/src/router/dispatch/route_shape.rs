//! Route-table shape used by lookup, listing, and read dispatch.
//!
//! [`Shape`] is a borrowed view over the compiled router that centralizes
//! route selection (per-kind `best_match` queries) and result assembly
//! (static lookups, projection-to-listing lowering, entry merging), so the
//! three entry points share one set of rules.

use crate::browse::{Entry as BrowseEntry, List, Listing, Lookup};
use crate::captures::Captures;
use crate::error::Result;
use crate::file_attrs::{FileProj, ReadMode, Size};
use crate::projection::{DirOutcome, DirProjection};
use omnifs_core::path::Path;

use super::super::compiled::CompiledRouter;
use super::super::handlers::{DirEntry, FileEntry};
use super::super::object::{ObjectReadTarget, ObjectRouteEntry, SourceLeafAttrs};
use super::super::pattern::best_match;

/// A borrowed dispatch view over the compiled route tables.
pub(in crate::router) struct Shape<'a, S> {
    pub(super) router: &'a CompiledRouter<S>,
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
        route: RouteMatch<'a, ObjectRouteEntry<S>>,
        target: ObjectReadTarget,
    },
}

impl<S> CompiledRouter<S> {
    pub(in crate::router) fn shape(&self) -> Shape<'_, S> {
        Shape { router: self }
    }
}

impl<S> Shape<'_, S> {
    /// Dir routes registered via `r.dir(..)`.
    pub(in crate::router) fn dir_route(&self, abs: &Path) -> Option<RouteMatch<'_, DirEntry<S>>> {
        route_match(self.router.dirs.iter(), abs)
    }

    /// File routes.
    pub(in crate::router) fn file_route(&self, abs: &Path) -> Option<RouteMatch<'_, FileEntry<S>>> {
        route_match(self.router.files.iter(), abs)
    }

    pub(super) fn object_route(&self, abs: &Path) -> Option<RouteMatch<'_, ObjectRouteEntry<S>>> {
        route_match(self.router.objects.iter(), abs)
    }

    /// Resolve a read path, in order:
    ///
    /// 1. a file route (`r.file`);
    /// 2. a file-object anchor (the abs path itself is the single-file object);
    /// 3. a leaf one level under a dir-object anchor, where the leaf resolves to
    ///    a canonical / representation / computed / direct / stream face.
    pub(super) fn read_route(&self, abs: &Path) -> Option<ReadRoute<'_, S>> {
        if let Some(route) = self.file_route(abs) {
            return Some(ReadRoute::File(route));
        }

        if let Some(route) = self.object_route(abs)
            && let Some(target) = route.entry.file_anchor_target()
        {
            return Some(ReadRoute::Object { route, target });
        }

        let (parent_abs, leaf) = abs.parent_and_name()?;
        let route = self.object_route(&parent_abs)?;
        let target = route.entry.read_target_for_leaf(leaf)?;
        Some(ReadRoute::Object { route, target })
    }

    /// A directory answer synthesized from the route table: no handler runs.
    /// Carries the parent's other static entries as siblings (one host
    /// round trip warms the whole directory) and is exhaustive only when no
    /// capture sibling can bind further names at this depth.
    pub(super) fn static_dir_lookup(&self, parent_abs: &Path, name: &str) -> Lookup {
        self.static_lookup(parent_abs, name, BrowseEntry::dir(name))
    }

    /// The file analog of [`Self::static_dir_lookup`]; the entry carries the
    /// listing-shape projection (size and bytes resolve at read time). A route
    /// declared `ranged` projects `ReadMode::Ranged` so the host dispatches
    /// `open` straight to `open-file`.
    pub(super) fn static_file_lookup(&self, parent_abs: &Path, name: &str, ranged: bool) -> Lookup {
        let shape = if ranged {
            FileProj::ranged_listing_shape()
        } else {
            FileProj::listing_shape()
        };
        self.static_lookup(parent_abs, name, BrowseEntry::file(name, shape))
    }

    fn static_lookup(&self, parent_abs: &Path, name: &str, target: BrowseEntry) -> Lookup {
        let siblings = self
            .static_entries_for_parent(parent_abs)
            .into_iter()
            .filter(|entry| entry.name() != name);
        Lookup::entry(target)
            .with_siblings(siblings)
            .exhaustive(!self.has_capture_child_under(parent_abs))
    }

    /// Resolve `name` against the visible children of an object anchored at
    /// `parent_abs`; not-found when no object is anchored there.
    pub(super) fn object_leaf_lookup(&self, parent_abs: &Path, name: &str) -> Lookup {
        let Some(route) = self.object_route(parent_abs) else {
            return Lookup::not_found();
        };
        let listing = self.object_dir_listing(route.entry, parent_abs, None);
        let Some(target) = listing
            .entries()
            .iter()
            .find(|entry| entry.name() == name)
            .cloned()
        else {
            return Lookup::not_found();
        };
        let siblings = listing
            .entries()
            .iter()
            .filter(|entry| entry.name() != name)
            .cloned();
        Lookup::entry(target).with_siblings(siblings)
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
            let merged = self.merge_entries(
                parent_abs,
                entries
                    .iter()
                    .map(crate::projection::Entry::to_browse_entry),
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
                let merged = self.merge_entries(
                    abs,
                    entries
                        .iter()
                        .map(crate::projection::Entry::to_browse_entry),
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
    /// A stream leaf (`o.file(..).stream(..)`) gets the ranged placeholder so
    /// the namespace read path opens it through `open-file` instead of routing
    /// a whole-file `read-file` the provider rejects.
    pub(super) fn object_dir_listing(
        &self,
        entry: &ObjectRouteEntry<S>,
        anchor_abs: &Path,
        source: Option<&SourceLeafAttrs>,
    ) -> Listing {
        let object_entries = entry.leaves.iter().map(|leaf| {
            if leaf.is_canonical()
                && let Some(source) = source
            {
                BrowseEntry::file(&leaf.name, source_leaf_shape(source))
            } else if leaf.is_stream() {
                BrowseEntry::file(&leaf.name, FileProj::ranged_listing_shape())
            } else {
                BrowseEntry::file(&leaf.name, FileProj::listing_shape())
            }
        });
        Listing::complete(self.merge_entries(anchor_abs, object_entries))
    }

    /// Merge dynamic entries over literal route-table siblings at the same
    /// depth. Dynamic entries win name collisions and the result is name
    /// ordered.
    fn merge_entries(
        &self,
        parent_abs: &Path,
        dynamic_entries: impl IntoIterator<Item = BrowseEntry>,
    ) -> Vec<BrowseEntry> {
        let mut entries = self
            .static_entries_for_parent(parent_abs)
            .into_iter()
            .map(|entry| (entry.name().to_string(), entry))
            .collect::<std::collections::BTreeMap<_, _>>();
        entries.extend(
            dynamic_entries
                .into_iter()
                .map(|entry| (entry.name().to_string(), entry)),
        );
        entries.into_values().collect()
    }
}

/// Merge the entries and effects of an ANCHOR-topology collection projection
/// into the parent anchor's listing. The collection enumerates child object
/// names (and emits each fresh child's canonical store through its projection
/// effects); those names become directory siblings of the parent's own leaves,
/// the parent's own leaves winning name collisions.
pub(in crate::router) fn merge_anchor_collection(
    listing: &Listing,
    projection: &DirProjection,
) -> Result<Listing> {
    let mut entries: Vec<BrowseEntry> = listing.entries().to_vec();
    let mut effects = listing.effects().clone();
    let parent_names: std::collections::BTreeSet<String> =
        entries.iter().map(|e| e.name().to_string()).collect();

    let mut all_exhaustive = listing.exhaustive();
    let mut next_cursor: Option<crate::handler::Cursor> = None;
    match projection.outcome() {
        DirOutcome::Entries {
            entries: child_entries,
            exhaustive,
            cursor,
        } => {
            for entry in child_entries {
                if !parent_names.contains(entry.name()) {
                    entries.push(entry.to_browse_entry());
                }
            }
            all_exhaustive &= *exhaustive;
            next_cursor.clone_from(cursor);
        },
        // A child listing whose validator matched: it contributes no fresh
        // entries here, but it is not a completeness claim either.
        DirOutcome::Unchanged => all_exhaustive = false,
    }
    let validator = projection.validator().map(|v| v.0.clone());
    effects.extend(projection.project_effects()?);

    let mut merged = if all_exhaustive {
        Listing::complete(entries)
    } else {
        Listing::partial(entries)
    }
    .with_effects(effects);
    if let Some(validator) = validator {
        merged = merged.with_validator(validator);
    }
    if let Some(cursor) = next_cursor {
        merged = merged.with_cursor(cursor);
    }
    Ok(merged)
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
