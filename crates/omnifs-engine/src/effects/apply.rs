//! Pure lowering from provider terminals to durable projection transitions.

use std::collections::BTreeMap;

use crate::cache::identity::GitId;
use crate::cache::{
    DirentsMutation, FactPayload, FeedPartWrite, FeedWrite, Freshness, GitWrite, Invalidation,
    MountResources, ObjectMutation, ProjectionTransition, RecordWrite,
};
use crate::clock::{DYNAMIC_TTL_MILLIS, freshness_expiry, page_deadline};
use crate::object_id::ObjectId;
use crate::ops::feed::{FeedChanges, FeedClaim, FeedObject, FeedOutcome};
use crate::ops::namespace::{
    DirEntry, DirListing, ListOutcome, PageRequest, ReadBytes, ReadOutcome,
};
use crate::view::{
    AttrPayload, DirentRecord, DirentsPage, DirentsPayload, EntryMeta, FileAttrsCache, FilePayload,
    ListingWrite, LookupPayload, PageValidator, Stability,
};
use crate::wit_protocol::{cached_cursor_from_wit, entry_meta_from_kind, stability_from_wit};
use omnifs_core::path::Path;
use omnifs_wit::host::types as wit_types;

#[derive(Debug, Clone)]
pub enum LookupOutcome {
    Entry(LookupEntry),
    Subtree(u64),
    NotFound,
    Feed(Vec<FeedClaim>),
}

#[derive(Debug, Clone)]
pub struct LookupEntry {
    pub(crate) path: Path,
    pub(crate) meta: EntryMeta,
}

impl LookupEntry {
    pub(crate) fn new(path: Path, meta: EntryMeta) -> Self {
        Self { path, meta }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn meta(&self) -> &EntryMeta {
        &self.meta
    }
}

pub struct EffectApplier<'a> {
    store: &'a MountResources,
}

impl<'a> EffectApplier<'a> {
    pub fn new(store: &'a MountResources) -> Self {
        Self { store }
    }

    pub(crate) fn lower_effects(
        &self,
        effects: &wit_types::Effects,
        now_millis: u64,
    ) -> anyhow::Result<ProjectionTransition> {
        self.lower_writes(effects, now_millis, false)
    }

    /// Lower the effects of a terminal. The writes of a feed part are fed
    /// facts: a file is a stable file whose bytes the projection holds, and
    /// no written path has a deadline.
    fn lower_writes(
        &self,
        effects: &wit_types::Effects,
        now_millis: u64,
        fed: bool,
    ) -> anyhow::Result<ProjectionTransition> {
        let mut transition = ProjectionTransition::default();
        for canonical in &effects.canonical {
            transition.objects.push(ObjectMutation::Canonical {
                id: ObjectId::from_wit(&canonical.id).as_bytes().to_vec(),
                bytes: canonical.bytes.clone(),
                validator: canonical.validator.clone(),
            });
            for alias in &canonical.view_leaves {
                transition.objects.push(ObjectMutation::Index {
                    id: ObjectId::from_wit(&canonical.id).as_bytes().to_vec(),
                    alias: Path::parse(alias).map_err(anyhow::Error::from)?,
                });
            }
        }

        let mut children = BTreeMap::<Path, BTreeMap<String, DirentRecord>>::new();
        let mut directory_writes = BTreeMap::<Path, ListingWrite>::new();
        for write in &effects.fs {
            let path = Path::parse(&write.path).map_err(anyhow::Error::from)?;
            let meta = match (&write.kind, fed) {
                (wit_types::FsKind::Directory(_), _) => EntryMeta::directory(),
                (wit_types::FsKind::File(file), true) => fed_file(&mut transition, &path, file)?,
                (wit_types::FsKind::File(file), false) => EntryMeta::file(self.file_meta(file)?),
            };
            if fed {
                add_lookup_records(&mut transition, &path, &meta);
                transition.freshness.push(Freshness {
                    path: path.clone(),
                    expires_at: None,
                });
            } else {
                self.add_entry_records(&mut transition, &path, &meta, &write.kind)?;
            }
            if let Some((parent, name)) = path.parent_and_name() {
                children.entry(parent).or_default().insert(
                    name.to_string(),
                    DirentRecord {
                        name: name.to_string(),
                        meta: meta.clone(),
                    },
                );
            }
            if let Some(id) = &write.id {
                transition.objects.push(ObjectMutation::Index {
                    id: ObjectId::from_wit(id).as_bytes().to_vec(),
                    alias: path.clone(),
                });
            }
            if let wit_types::FsKind::Directory(write) = &write.kind {
                directory_writes.insert(path.clone(), listing_write_from_wit(*write));
                if !fed {
                    transition.freshness.push(Freshness {
                        path,
                        expires_at: freshness_expiry(
                            stability_from_wit(wit_types::Stability::Dynamic),
                            now_millis,
                        ),
                    });
                }
            }
        }
        for (path, write) in directory_writes {
            let entries = children.remove(&path).unwrap_or_default();
            transition.dirents.push(DirentsMutation::MergeHints {
                path,
                entries: entries.into_values().collect(),
                write,
            });
        }
        for removal in &effects.removals {
            transition
                .removals
                .push(Path::parse(removal).map_err(anyhow::Error::from)?);
        }
        for invalidation in &effects.invalidations {
            transition.invalidations.push(match invalidation {
                wit_types::Invalidation::Object(id) => {
                    Invalidation::Object(ObjectId::from_wit(id).as_bytes().to_vec())
                },
                wit_types::Invalidation::Listing(wit_types::PathOrPrefix::Path(path)) => {
                    Invalidation::ListingPath(Path::parse(path).map_err(anyhow::Error::from)?)
                },
                wit_types::Invalidation::Listing(wit_types::PathOrPrefix::Prefix(path)) => {
                    Invalidation::ListingPrefix(Path::parse(path).map_err(anyhow::Error::from)?)
                },
            });
        }
        Ok(transition)
    }

    fn file_meta(&self, file: &wit_types::FileOut) -> anyhow::Result<FileAttrsCache> {
        crate::wit_protocol::try_file_attrs_from_file_out(file, |handle| {
            self.store
                .body_for_handle(handle)
                .map_err(|error| error.to_string())
        })
        .map_err(anyhow::Error::msg)
    }

    fn add_entry_records(
        &self,
        transition: &mut ProjectionTransition,
        path: &Path,
        meta: &EntryMeta,
        kind: &wit_types::FsKind,
    ) -> anyhow::Result<()> {
        add_lookup_records(transition, path, meta);
        if let wit_types::FsKind::File(file) = kind {
            let attrs = self.file_meta(file)?;
            if let Some(bytes) = attrs.inline_bytes()
                && let Some(aux) = attrs.durable_observation_cache_aux()
            {
                transition.records.push(RecordWrite {
                    path: path.clone(),
                    aux,
                    fact: FactPayload::File(
                        FilePayload::new(attrs.version_token_owned(), bytes.to_vec())
                            .with_content_type(file.content_type.clone()),
                    ),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn lower_lookup(
        &self,
        parent: &Path,
        child: &Path,
        result: wit_types::LookupChildResult,
        now_millis: u64,
        resolve_tree: impl Fn(u64) -> Option<GitId>,
    ) -> anyhow::Result<(LookupOutcome, ProjectionTransition)> {
        let mut transition = ProjectionTransition::default();
        let outcome = match result {
            wit_types::LookupChildResult::Entry(entry) => {
                let mut hints = Vec::new();
                let mut target_meta = None;
                for candidate in std::iter::once(&entry.target).chain(entry.siblings.iter()) {
                    let path = parent.join(&candidate.name).map_err(anyhow::Error::msg)?;
                    let meta = entry_meta_from_kind(&candidate.kind, |handle| {
                        self.store
                            .body_for_handle(handle)
                            .map_err(|error| error.to_string())
                    })
                    .map_err(anyhow::Error::msg)?;
                    let kind = match &candidate.kind {
                        wit_types::EntryKind::Directory => {
                            wit_types::FsKind::Directory(wit_types::ListingWrite::Partial)
                        },
                        wit_types::EntryKind::File(file) => wit_types::FsKind::File(file.clone()),
                    };
                    self.add_entry_records(&mut transition, &path, &meta, &kind)?;
                    if let Some(id) = &candidate.id {
                        transition.objects.push(ObjectMutation::Index {
                            id: ObjectId::from_wit(id).as_bytes().to_vec(),
                            alias: path,
                        });
                    }
                    hints.push(DirentRecord {
                        name: candidate.name.clone(),
                        meta: meta.clone(),
                    });
                    if candidate.name == entry.target.name {
                        target_meta = Some(meta);
                    }
                }
                transition.dirents.push(DirentsMutation::MergeHints {
                    path: parent.clone(),
                    entries: hints,
                    write: if entry.exhaustive {
                        ListingWrite::Exhaustive
                    } else {
                        ListingWrite::Partial
                    },
                });
                target_meta.map_or(LookupOutcome::NotFound, |meta| {
                    LookupOutcome::Entry(LookupEntry::new(child.clone(), meta))
                })
            },
            wit_types::LookupChildResult::Subtree(tree) => {
                let id = resolve_tree(tree)
                    .ok_or_else(|| anyhow::anyhow!("unresolved Git tree handle {tree}"))?;
                transition.git.push(GitWrite {
                    path: child.clone(),
                    id,
                    relative_path: String::new(),
                });
                transition.records.push(RecordWrite {
                    path: child.clone(),
                    aux: None,
                    fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                });
                LookupOutcome::Subtree(tree)
            },
            wit_types::LookupChildResult::NotFound(id) => {
                transition.records.push(RecordWrite {
                    path: child.clone(),
                    aux: None,
                    fact: FactPayload::Lookup(LookupPayload::Negative {
                        id: id.map(|value| ObjectId::from_wit(&value).as_bytes().to_vec()),
                    }),
                });
                transition.freshness.push(Freshness {
                    path: child.clone(),
                    expires_at: Some(now_millis.saturating_add(DYNAMIC_TTL_MILLIS)),
                });
                LookupOutcome::NotFound
            },
            wit_types::LookupChildResult::Feed(claims) => {
                LookupOutcome::Feed(feed_claims(&claims)?)
            },
        };
        Ok((outcome, transition))
    }

    // One typed provider terminal lowers into one coherent projection
    // transition; helper carriers would only duplicate that transactional state.
    //
    // The asked page lowers by its request: a first page without a validator
    // replaces the listing, a later page without one appends to it, and a
    // page asked for with its validator replaces or renews that cached page.
    // A page with a validator has the deadline of its interval.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn lower_list(
        &self,
        path: &Path,
        result: wit_types::ListChildrenResult,
        request: &PageRequest,
        now_millis: u64,
        resolve_tree: impl Fn(u64) -> Option<GitId>,
    ) -> anyhow::Result<(ListOutcome, ProjectionTransition)> {
        let mut transition = ProjectionTransition::default();
        let outcome = match result {
            wit_types::ListChildrenResult::Entries(listing) => {
                let mut entries = Vec::new();
                for entry in &listing.entries {
                    let child = path.join(&entry.name).map_err(anyhow::Error::msg)?;
                    let meta = entry_meta_from_kind(&entry.kind, |handle| {
                        self.store
                            .body_for_handle(handle)
                            .map_err(|error| error.to_string())
                    })
                    .map_err(anyhow::Error::msg)?;
                    let kind = match &entry.kind {
                        wit_types::EntryKind::Directory => {
                            wit_types::FsKind::Directory(wit_types::ListingWrite::Partial)
                        },
                        wit_types::EntryKind::File(file) => wit_types::FsKind::File(file.clone()),
                    };
                    self.add_entry_records(&mut transition, &child, &meta, &kind)?;
                    if let Some(id) = &entry.id {
                        transition.objects.push(ObjectMutation::Index {
                            id: ObjectId::from_wit(id).as_bytes().to_vec(),
                            alias: child,
                        });
                    }
                    entries.push(DirentRecord {
                        name: entry.name.clone(),
                        meta,
                    });
                }
                let next_cursor = listing.next_cursor.clone().map(cached_cursor_from_wit);
                let mut page_entries = entries.clone();
                if request.cursor.is_none() && next_cursor.is_some() {
                    page_entries.extend(crate::tree::synthetic::control_entries());
                }
                let page = DirentsPage {
                    cursor: request.cursor.clone(),
                    validator: listing.validator.as_ref().map(|validator| PageValidator {
                        token: validator.token.clone(),
                        interval: validator.interval,
                        deadline: page_deadline(validator.interval, now_millis),
                    }),
                    entries: page_entries,
                };
                let exhaustive = listing.exhaustive && next_cursor.is_none();
                transition
                    .dirents
                    .push(match (&request.cursor, &request.validator) {
                        (_, Some(_)) => DirentsMutation::ReplacePage {
                            path: path.clone(),
                            page,
                            next_cursor: next_cursor.clone(),
                            exhaustive,
                        },
                        (None, None) => DirentsMutation::Replace {
                            path: path.clone(),
                            value: DirentsPayload {
                                pages: vec![page],
                                exhaustive,
                                next_cursor: next_cursor.clone(),
                            },
                        },
                        (Some(_), None) => DirentsMutation::AppendPage {
                            path: path.clone(),
                            page,
                            next_cursor: next_cursor.clone(),
                            exhaustive,
                        },
                    });
                transition.freshness.push(Freshness {
                    path: path.clone(),
                    expires_at: freshness_expiry(
                        if listing.exhaustive {
                            Stability::Stable
                        } else {
                            Stability::Dynamic
                        },
                        now_millis,
                    ),
                });
                ListOutcome::Entries(DirListing {
                    entries: entries
                        .into_iter()
                        .map(|entry| DirEntry {
                            name: entry.name,
                            meta: entry.meta,
                        })
                        .collect(),
                    next_cursor,
                })
            },
            wit_types::ListChildrenResult::Subtree(tree) => {
                let id = resolve_tree(tree)
                    .ok_or_else(|| anyhow::anyhow!("unresolved Git tree handle {tree}"))?;
                transition.git.push(GitWrite {
                    path: path.clone(),
                    id,
                    relative_path: String::new(),
                });
                transition.records.push(RecordWrite {
                    path: path.clone(),
                    aux: None,
                    fact: FactPayload::Lookup(LookupPayload::Positive(EntryMeta::directory())),
                });
                ListOutcome::Subtree(tree)
            },
            wit_types::ListChildrenResult::Unchanged(interval) => {
                transition.dirents.push(DirentsMutation::RenewPage {
                    path: path.clone(),
                    cursor: request.cursor.clone(),
                    interval,
                    now_millis,
                });
                ListOutcome::Unchanged
            },
            wit_types::ListChildrenResult::Feed(claims) => ListOutcome::Feed(feed_claims(&claims)?),
        };
        Ok((outcome, transition))
    }

    /// Lower one answer of the feed of `object`: the facts of a part are fed,
    /// and the feed states it brought publish with them. Returns the outcome
    /// and what the answer changes.
    pub(crate) fn lower_feed(
        &self,
        object: &FeedObject,
        answer: wit_types::FeedAnswer,
        effects: &wit_types::Effects,
        now_millis: u64,
    ) -> anyhow::Result<(FeedOutcome, ProjectionTransition, FeedChanges)> {
        let mut transition = self.lower_writes(effects, now_millis, true)?;
        let mut changes = FeedChanges::default();
        for write in &effects.fs {
            let path = Path::parse(&write.path).map_err(anyhow::Error::from)?;
            if let Some((parent, _)) = path.parent_and_name() {
                changes.listings.push(parent);
            }
            match write.kind {
                wit_types::FsKind::Directory(_) => changes.listings.push(path),
                wit_types::FsKind::File(_) => changes.paths.push(path),
            }
        }
        for path in &transition.removals {
            if let Some((parent, _)) = path.parent_and_name() {
                changes.listings.push(parent);
            }
            changes.paths.push(path.clone());
        }
        for paths in [&mut changes.listings, &mut changes.paths] {
            paths.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            paths.dedup();
        }
        let deadline = now_millis.saturating_add(DYNAMIC_TTL_MILLIS);
        let (outcome, part) = match answer {
            wit_types::FeedAnswer::Unchanged => (FeedOutcome::Unchanged, None),
            wit_types::FeedAnswer::Part(part) => {
                let inner = part
                    .inner
                    .iter()
                    .map(|state| {
                        (
                            ObjectId::from_wit(&state.object.id).as_bytes().to_vec(),
                            state.token.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                (
                    FeedOutcome::Part { more: part.more },
                    Some((
                        FeedPartWrite {
                            token: part.token,
                            more: part.more,
                        },
                        inner,
                    )),
                )
            },
        };
        let (part, inner) = part.map_or((None, Vec::new()), |(part, inner)| (Some(part), inner));
        transition.feed = Some(FeedWrite {
            object: object.id.as_bytes().to_vec(),
            part,
            inner,
            deadline,
        });
        Ok((outcome, transition, changes))
    }

    pub(crate) fn lower_read(
        &self,
        path: &Path,
        value: wit_types::ReadFileResult,
        staged_objects: &[ObjectMutation],
    ) -> anyhow::Result<(ReadOutcome, ProjectionTransition)> {
        let outcome = ReadOutcome::from_wit(value, |handle| {
            self.store
                .body_for_handle(handle)
                .map_err(|error| error.to_string())
        })
        .map_err(anyhow::Error::msg)?;
        let mut transition = ProjectionTransition::default();
        let mut attrs = outcome.attrs.clone();
        let durable_file = match &outcome.bytes {
            ReadBytes::Inline(bytes) => Some(FactPayload::File(
                FilePayload::new(attrs.version_token_owned(), bytes.clone())
                    .with_content_type(outcome.content_type.clone()),
            )),
            ReadBytes::Body(body) => {
                let length = match attrs.size() {
                    crate::view::FileSize::Exact(length) => length,
                    crate::view::FileSize::NonZero | crate::view::FileSize::Unknown => {
                        anyhow::bail!("blob-backed read result lacks a trusted exact size")
                    },
                };
                Some(FactPayload::FileBody {
                    version_token: attrs.version_token_owned(),
                    content_type: outcome.content_type.clone(),
                    body: *body,
                    length,
                })
            },
            ReadBytes::Canonical => {
                let canonical = self
                    .store
                    .selected_canonical_for(path, staged_objects)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?
                    .ok_or_else(|| anyhow::anyhow!("canonical read has no selected object"))?;
                let length = u64::try_from(canonical.bytes.len())
                    .map_err(|_| anyhow::anyhow!("canonical body length does not fit u64"))?;
                match attrs.size() {
                    crate::view::FileSize::Exact(expected) if expected != length => {
                        anyhow::bail!(
                            "canonical body length {length} disagrees with declared size {expected}"
                        )
                    },
                    crate::view::FileSize::NonZero if length == 0 => {
                        anyhow::bail!("canonical body is empty despite NonZero size")
                    },
                    _ => {},
                }
                attrs = crate::view::FileAttrsCache::from_parts(
                    crate::view::FileSize::Exact(length),
                    crate::view::ByteSource::Canonical,
                    attrs.stability(),
                    attrs.version_token_owned(),
                )
                .map_err(|error| anyhow::anyhow!(error))?;
                Some(FactPayload::FileBody {
                    version_token: attrs.version_token_owned(),
                    content_type: outcome.content_type.clone(),
                    body: crate::view::BodyId::from_bytes(&canonical.bytes),
                    length,
                })
            },
        };
        let meta = EntryMeta::file(attrs.clone());
        transition.records.push(RecordWrite {
            path: path.clone(),
            aux: None,
            fact: FactPayload::Lookup(LookupPayload::Positive(meta.clone())),
        });
        transition.records.push(RecordWrite {
            path: path.clone(),
            aux: None,
            fact: FactPayload::Attr(AttrPayload { meta }),
        });
        if let Some(file) = durable_file
            && let Some(aux) = attrs.durable_observation_cache_aux()
        {
            transition.records.push(RecordWrite {
                path: path.clone(),
                aux,
                fact: file,
            });
        }
        transition.freshness.push(Freshness {
            path: path.clone(),
            expires_at: freshness_expiry(attrs.stability(), crate::clock::now_millis()),
        });
        Ok((outcome, transition))
    }
}

fn add_lookup_records(transition: &mut ProjectionTransition, path: &Path, meta: &EntryMeta) {
    transition.records.push(RecordWrite {
        path: path.clone(),
        aux: None,
        fact: FactPayload::Lookup(LookupPayload::Positive(meta.clone())),
    });
    transition.records.push(RecordWrite {
        path: path.clone(),
        aux: None,
        fact: FactPayload::Attr(AttrPayload { meta: meta.clone() }),
    });
}

/// A fed file: a stable file of its exact size whose bytes the projection
/// holds in its file record, so a read serves it whole without the eager cap
/// of an inline preload.
fn fed_file(
    transition: &mut ProjectionTransition,
    path: &Path,
    file: &wit_types::FileOut,
) -> anyhow::Result<EntryMeta> {
    let wit_types::ByteSource::Inline(bytes) = &file.bytes else {
        anyhow::bail!("fed file {path} has no inline bytes");
    };
    let attrs = FileAttrsCache::deferred(
        crate::view::FileSize::Exact(u64::try_from(bytes.len())?),
        crate::view::ReadMode::Full,
        Stability::Stable,
        None,
    )
    .map_err(anyhow::Error::msg)?;
    transition.records.push(RecordWrite {
        path: path.clone(),
        aux: None,
        fact: FactPayload::File(
            FilePayload::new(None, bytes.clone()).with_content_type(file.content_type.clone()),
        ),
    });
    Ok(EntryMeta::file(attrs))
}

fn listing_write_from_wit(write: wit_types::ListingWrite) -> ListingWrite {
    match write {
        wit_types::ListingWrite::Partial => ListingWrite::Partial,
        wit_types::ListingWrite::Exhaustive => ListingWrite::Exhaustive,
        wit_types::ListingWrite::Complete => ListingWrite::Complete,
    }
}

fn feed_claims(claims: &[wit_types::FeedClaim]) -> anyhow::Result<Vec<FeedClaim>> {
    claims.iter().map(FeedClaim::from_wit).collect()
}
