//! Typed provider operation execution and settled terminal publication.

use crate::Runtime;
use crate::clock;
use crate::effect_apply::EffectApplier;
use crate::effect_apply::LookupOutcome;
use crate::inspector;
use crate::ops::namespace::{ChunkOutcome, ListOutcome, OpenOutcome, PageRequest, ReadReply};
use crate::runtime::{EngineError, Result};
use omnifs_api::events::InspectorOutcome;
use omnifs_core::path::{Path, Segment};
use omnifs_wit::host::types as wit_types;
use tracing::Instrument;

impl Runtime {
    pub(crate) async fn run_lookup_child(
        &self,
        parent_path: &Path,
        name: &Segment,
        captured_epoch: u64,
    ) -> Result<LookupOutcome> {
        let id = self.next_operation_id();
        let child_path = parent_path.join_segment(name);
        let span = inspector::provider_span(
            id,
            &self.mount_name,
            &self.provider_name,
            "lookup_child",
            child_path.as_str(),
        );
        let blob_guard = self.resources.blob_publication(id);
        async {
            let (wire_result, effects) = self
                .instance
                .lookup_child(
                    id,
                    parent_path.as_str().to_string(),
                    name.as_str().to_string(),
                )
                .await?;
            crate::op_validate::validate_lookup(&wire_result, &effects, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &wire_result);
            let pending_blobs = blob_guard.take();
            let wire_result = self.provider_result(wire_result)?;
            let applier = EffectApplier::new(&self.resources);
            let mut transition = applier
                .lower_effects(&effects, clock::now_millis())
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            transition.blobs.extend(pending_blobs);
            let (result, result_transition) = applier
                .lower_lookup(
                    parent_path,
                    &child_path,
                    wire_result,
                    clock::now_millis(),
                    |tree| self.tree_ref(tree).map(|reference| reference.id),
                )
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            merge_transition(&mut transition, result_transition);
            self.publish_transition(transition, captured_epoch)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            if let LookupOutcome::Subtree(tree) = result {
                inspector::record_subtree_handoff(id, tree);
            }
            Ok(result)
        }
        .instrument(span.clone())
        .await
    }

    pub(crate) async fn run_list_children(
        &self,
        path: &Path,
        request: PageRequest,
        captured_epoch: u64,
    ) -> Result<ListOutcome> {
        let id = self.next_operation_id();
        let span = inspector::provider_span(
            id,
            &self.mount_name,
            &self.provider_name,
            "list_children",
            path.as_str(),
        );
        let blob_guard = self.resources.blob_publication(id);
        async {
            let (wire_result, effects) = self
                .instance
                .list_children(
                    id,
                    path.as_str().to_string(),
                    request
                        .cursor
                        .clone()
                        .map(crate::wit_protocol::cached_cursor_to_wit),
                    request.validator.clone(),
                )
                .await?;
            crate::op_validate::validate_list(
                &wire_result,
                &effects,
                request.validator.is_some(),
                |tree| self.tree_ref(tree).is_some(),
            )
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &wire_result);
            let pending_blobs = blob_guard.take();
            let wire_result = self.provider_result(wire_result)?;
            let applier = EffectApplier::new(&self.resources);
            let now_millis = clock::now_millis();
            let mut transition = applier
                .lower_effects(&effects, now_millis)
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            transition.blobs.extend(pending_blobs);
            let (result, result_transition) = applier
                .lower_list(path, wire_result, &request, now_millis, |tree| {
                    self.tree_ref(tree).map(|reference| reference.id)
                })
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            merge_transition(&mut transition, result_transition);
            self.publish_transition(transition, captured_epoch)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            if let ListOutcome::Subtree(tree) = result {
                inspector::record_subtree_handoff(id, tree);
            }
            Ok(result)
        }
        .instrument(span.clone())
        .await
    }

    pub(crate) async fn run_read_file(
        &self,
        path: &Path,
        content_type: String,
        cached_canonical: Option<wit_types::CanonicalInput>,
        captured_epoch: u64,
    ) -> Result<ReadReply> {
        let id = self.next_operation_id();
        let span = inspector::provider_span(
            id,
            &self.mount_name,
            &self.provider_name,
            "read_file",
            path.as_str(),
        );
        let blob_guard = self.resources.blob_publication(id);
        async {
            let (wire_result, effects) = self
                .instance
                .read_file(
                    id,
                    path.as_str().to_string(),
                    content_type,
                    cached_canonical,
                )
                .await?;
            crate::op_validate::validate_read(&wire_result, &effects, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &wire_result);
            let pending_blobs = blob_guard.take();
            let wire_result = self.provider_result(wire_result)?;
            let applier = EffectApplier::new(&self.resources);
            let mut transition = applier
                .lower_effects(&effects, clock::now_millis())
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            let result = match wire_result {
                wit_types::ReadFileOutcome::Found(value) => {
                    let (result, result_transition) = applier
                        .lower_read(path, value, &transition.objects)
                        .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
                    merge_transition(&mut transition, result_transition);
                    transition.blobs.extend(pending_blobs);
                    ReadReply::File(result)
                },
                wit_types::ReadFileOutcome::Feed(claims) => ReadReply::Feed(
                    claims
                        .iter()
                        .map(crate::ops::feed::FeedClaim::from_wit)
                        .collect::<anyhow::Result<Vec<_>>>()
                        .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?,
                ),
                wit_types::ReadFileOutcome::NotFound(maybe_id) => {
                    drop(pending_blobs);
                    transition.records.push(crate::cache::RecordWrite {
                        path: path.clone(),
                        aux: None,
                        fact: crate::cache::FactPayload::Lookup(
                            crate::view::LookupPayload::Negative {
                                id: maybe_id.as_ref().map(|value| {
                                    crate::object_id::ObjectId::from_wit(value)
                                        .as_bytes()
                                        .to_vec()
                                }),
                            },
                        ),
                    });
                    transition.freshness.push(crate::cache::Freshness {
                        path: path.clone(),
                        expires_at: Some(
                            clock::now_millis().saturating_add(clock::DYNAMIC_TTL_MILLIS),
                        ),
                    });
                    self.publish_transition(transition, captured_epoch)?;
                    inspector::record_outcome(&span, InspectorOutcome::NotFound);
                    return Err(EngineError::ProviderError(wit_types::ProviderError {
                        kind: wit_types::ErrorKind::NotFound,
                        message: format!("no such file: {path}"),
                        retryable: false,
                        retry_after: None,
                    }));
                },
            };
            self.publish_transition(transition, captured_epoch)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            Ok(result)
        }
        .instrument(span.clone())
        .await
    }

    pub(crate) async fn run_open_file(
        &self,
        path: &Path,
        captured_epoch: u64,
    ) -> Result<OpenOutcome> {
        let id = self.next_operation_id();
        let span = inspector::provider_span(
            id,
            &self.mount_name,
            &self.provider_name,
            "open_file",
            path.as_str(),
        );
        let blob_guard = self.resources.blob_publication(id);
        async {
            let (wire_result, effects) = self
                .instance
                .open_file(id, path.as_str().to_string())
                .await?;
            crate::op_validate::validate_open(&wire_result, &effects, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &wire_result);
            let pending_blobs = blob_guard.take();
            let wire_result = self.provider_result(wire_result)?;
            let outcome = OpenOutcome::from_wit(wire_result);
            let meta = crate::view::EntryMeta::file(outcome.attrs.clone());
            let mut transition = EffectApplier::new(&self.resources)
                .lower_effects(&effects, clock::now_millis())
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            transition.blobs.extend(pending_blobs);
            transition.records.extend([
                crate::cache::RecordWrite {
                    path: path.clone(),
                    aux: None,
                    fact: crate::cache::FactPayload::Lookup(crate::view::LookupPayload::Positive(
                        meta.clone(),
                    )),
                },
                crate::cache::RecordWrite {
                    path: path.clone(),
                    aux: None,
                    fact: crate::cache::FactPayload::Attr(crate::view::AttrPayload { meta }),
                },
            ]);
            self.publish_transition(transition, captured_epoch)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            Ok(outcome)
        }
        .instrument(span.clone())
        .await
    }

    pub(crate) async fn run_read_chunk(
        &self,
        path: Option<&Path>,
        attrs: Option<&crate::view::FileAttrsCache>,
        handle: u64,
        offset: u64,
        length: u32,
        captured_epoch: u64,
    ) -> Result<ChunkOutcome> {
        let id = self.next_operation_id();
        let span =
            inspector::provider_span(id, &self.mount_name, &self.provider_name, "read_chunk", "");
        let blob_guard = self.resources.blob_publication(id);
        async {
            let (wire_result, effects) =
                self.instance.read_chunk(id, handle, offset, length).await?;
            crate::op_validate::validate_chunk(&wire_result, &effects, length, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &wire_result);
            let pending_blobs = blob_guard.take();
            let wire_result = self.provider_result(wire_result)?;
            let mut transition = EffectApplier::new(&self.resources)
                .lower_effects(&effects, clock::now_millis())
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            transition.blobs.extend(pending_blobs);
            let mut outcome = ChunkOutcome::from_wit(wire_result);
            if let (Some(path), Some(attrs)) = (path, attrs)
                && outcome.eof
            {
                let content_len = u64::try_from(outcome.content.len()).map_err(|_| {
                    EngineError::ProviderProtocol("chunk length does not fit u64".into())
                })?;
                let eof_size = offset.checked_add(content_len).ok_or_else(|| {
                    EngineError::ProviderProtocol("ranged EOF offset overflow".into())
                })?;
                let learned = if matches!(attrs.stability(), crate::view::Stability::Live) {
                    Some(attrs.clone().with_exact_size(eof_size))
                } else {
                    attrs
                        .learned_ranged_eof_attrs(eof_size)
                        .map_err(EngineError::ProviderProtocol)?
                };
                if let Some(learned) = learned.clone() {
                    let meta = crate::view::EntryMeta::file(learned.clone());
                    transition.records.extend([
                        crate::cache::RecordWrite {
                            path: path.clone(),
                            aux: None,
                            fact: crate::cache::FactPayload::Lookup(
                                crate::view::LookupPayload::Positive(meta.clone()),
                            ),
                        },
                        crate::cache::RecordWrite {
                            path: path.clone(),
                            aux: None,
                            fact: crate::cache::FactPayload::Attr(crate::view::AttrPayload {
                                meta,
                            }),
                        },
                    ]);
                    outcome.learned_attrs = Some(learned);
                }
            }
            self.publish_transition(transition, captured_epoch)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            Ok(outcome)
        }
        .instrument(span.clone())
        .await
    }

    pub(crate) async fn run_event(
        &self,
        event: wit_types::ProviderEvent,
        captured_epoch: u64,
    ) -> Result<()> {
        let id = self.next_operation_id();
        let span =
            inspector::provider_span(id, &self.mount_name, &self.provider_name, "on_event", "");
        let blob_guard = self.resources.blob_publication(id);
        async {
            let (wire_result, effects) = self.instance.on_event(id, event).await?;
            crate::op_validate::validate_event(&wire_result, &effects, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &wire_result);
            let pending_blobs = blob_guard.take();
            self.provider_result(wire_result)?;
            let transition = EffectApplier::new(&self.resources)
                .lower_effects(&effects, clock::now_millis())
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            let mut transition = transition;
            transition.blobs.extend(pending_blobs);
            self.publish_transition(transition, captured_epoch)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            Ok(())
        }
        .instrument(span.clone())
        .await
    }

    pub(crate) fn publish_transition(
        &self,
        transition: crate::cache::ProjectionTransition,
        captured_epoch: u64,
    ) -> Result<()> {
        let outcome = self
            .resources
            .publish(transition, captured_epoch)
            .map_err(|error| {
                EngineError::ProviderProtocol(format!("cache publication failed: {error}"))
            })?;
        match outcome {
            crate::cache::PublicationOutcome::Committed { invalidations } => {
                let mut prefixes = Vec::new();
                let mut paths = Vec::new();
                for invalidation in invalidations {
                    match invalidation {
                        crate::cache::Invalidation::ListingPath(path) => paths.push(path),
                        crate::cache::Invalidation::ListingPrefix(path) => prefixes.push(path),
                        crate::cache::Invalidation::Object(_) => {},
                    }
                }
                self.record_view_invalidations(prefixes, paths);
                Ok(())
            },
            crate::cache::PublicationOutcome::Fenced => Err(EngineError::ProviderProtocol(
                "cache publication crossed an invalidation fence".into(),
            )),
        }
    }

    pub(super) fn provider_result<T>(
        &self,
        result: std::result::Result<T, wit_types::ProviderError>,
    ) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if error.kind == wit_types::ErrorKind::RateLimited {
                    self.note_rate_limited(
                        error
                            .retry_after
                            .map(u64::from)
                            .map(std::time::Duration::from_secs),
                    );
                }
                Err(EngineError::ProviderError(error))
            },
        }
    }
}

fn merge_transition(
    target: &mut crate::cache::ProjectionTransition,
    mut addition: crate::cache::ProjectionTransition,
) {
    target.records.append(&mut addition.records);
    for mutation in addition.dirents {
        merge_dirents_mutation(&mut target.dirents, mutation);
    }
    target.objects.append(&mut addition.objects);
    target.freshness.append(&mut addition.freshness);
    target.invalidations.append(&mut addition.invalidations);
    target.blobs.append(&mut addition.blobs);
    target.git.append(&mut addition.git);
    target.removals.append(&mut addition.removals);
}

/// The listing write of two merged hint batches: an exhaustive batch
/// replaces the listing, else a complete one completes it.
fn stronger_write(
    left: crate::view::ListingWrite,
    right: crate::view::ListingWrite,
) -> crate::view::ListingWrite {
    use crate::view::ListingWrite;
    match (left, right) {
        (ListingWrite::Exhaustive, _) | (_, ListingWrite::Exhaustive) => ListingWrite::Exhaustive,
        (ListingWrite::Complete, _) | (_, ListingWrite::Complete) => ListingWrite::Complete,
        (ListingWrite::Partial, ListingWrite::Partial) => ListingWrite::Partial,
    }
}

fn merge_hint_entries(
    base: &mut Vec<crate::view::DirentRecord>,
    additions: impl IntoIterator<Item = crate::view::DirentRecord>,
    overwrite: bool,
) {
    for addition in additions {
        if let Some(existing) = base.iter_mut().find(|entry| entry.name == addition.name) {
            if overwrite {
                *existing = addition;
            }
        } else {
            base.push(addition);
        }
    }
}

fn merge_dirents_mutation(
    target: &mut Vec<crate::cache::DirentsMutation>,
    addition: crate::cache::DirentsMutation,
) {
    let Some(index) = target
        .iter()
        .position(|existing| existing.path() == addition.path())
    else {
        target.push(addition);
        return;
    };

    let existing = target.remove(index);
    let merged = match (existing, addition) {
        (
            crate::cache::DirentsMutation::MergeHints {
                path,
                mut entries,
                write,
            },
            crate::cache::DirentsMutation::MergeHints {
                entries: additions,
                write: addition_write,
                ..
            },
        ) => {
            merge_hint_entries(&mut entries, additions, true);
            crate::cache::DirentsMutation::MergeHints {
                path,
                entries,
                write: stronger_write(write, addition_write),
            }
        },
        (
            crate::cache::DirentsMutation::MergeHints { entries, .. },
            crate::cache::DirentsMutation::Replace { path, mut value },
        )
        | (
            crate::cache::DirentsMutation::Replace { path, mut value },
            crate::cache::DirentsMutation::MergeHints { entries, .. },
        ) => {
            if let Some(page) = value.pages.last_mut() {
                merge_hint_entries(&mut page.entries, entries, false);
            }
            crate::cache::DirentsMutation::Replace { path, value }
        },
        (
            crate::cache::DirentsMutation::MergeHints { entries, .. },
            crate::cache::DirentsMutation::AppendPage {
                path,
                mut page,
                next_cursor,
                exhaustive,
            },
        )
        | (
            crate::cache::DirentsMutation::AppendPage {
                path,
                mut page,
                next_cursor,
                exhaustive,
            },
            crate::cache::DirentsMutation::MergeHints { entries, .. },
        ) => {
            merge_hint_entries(&mut page.entries, entries, false);
            crate::cache::DirentsMutation::AppendPage {
                path,
                page,
                next_cursor,
                exhaustive,
            }
        },
        (
            crate::cache::DirentsMutation::MergeHints { entries, .. },
            crate::cache::DirentsMutation::ReplacePage {
                path,
                mut page,
                next_cursor,
                exhaustive,
            },
        )
        | (
            crate::cache::DirentsMutation::ReplacePage {
                path,
                mut page,
                next_cursor,
                exhaustive,
            },
            crate::cache::DirentsMutation::MergeHints { entries, .. },
        ) => {
            merge_hint_entries(&mut page.entries, entries, false);
            crate::cache::DirentsMutation::ReplacePage {
                path,
                page,
                next_cursor,
                exhaustive,
            }
        },
        (existing, addition) => {
            target.insert(index, existing);
            target.push(addition);
            return;
        },
    };
    target.insert(index, merged);
}

pub(super) fn inspect_result<T>(
    span: &tracing::Span,
    result: &std::result::Result<T, wit_types::ProviderError>,
) {
    if let Err(error) = result {
        inspector::record_outcome(span, inspector::outcome_for_provider_error(error));
    }
}
