//! Typed provider operation execution and settled terminal publication.

use crate::Runtime;
use crate::cache::BlobPublicationGuard;
use crate::clock;
use crate::effect_apply::EffectApplier;
use crate::effect_apply::LookupOutcome;
use crate::inspector;
use crate::ops::namespace::{ChunkOutcome, ListOutcome, OpenOutcome, ReadOutcome};
use crate::runtime::{EngineError, Result};
use omnifs_api::events::InspectorOutcome;
use omnifs_core::path::{Path, Segment};
use omnifs_wit::host::types as wit_types;
use tracing::Instrument;

/// Owns the shared terminal path after a provider operation returns.
///
/// Validation runs before any effect lowering, staged blob writes stay owned by
/// this value until publication, and one fenced projection commit closes the
/// Inspector span. Operation-specific lowering remains at each caller because
/// lookup/list hand off subtrees and read/chunk have distinct cache policies.
struct TerminalTransaction<'a> {
    runtime: &'a Runtime,
    span: tracing::Span,
    captured_epoch: u64,
    transition: crate::cache::ProjectionTransition,
}

impl<'a> TerminalTransaction<'a> {
    fn prepare<T, F>(
        runtime: &'a Runtime,
        span: tracing::Span,
        captured_epoch: u64,
        blob_guard: BlobPublicationGuard<'a>,
        wire_result: std::result::Result<T, wit_types::ProviderError>,
        effects: &wit_types::Effects,
        validate: F,
    ) -> Result<(T, Self)>
    where
        F: FnOnce(
            &std::result::Result<T, wit_types::ProviderError>,
            &wit_types::Effects,
        ) -> std::result::Result<(), String>,
    {
        validate(&wire_result, effects).map_err(EngineError::ProviderProtocol)?;
        inspect_result(&span, &wire_result);
        let pending_blobs = blob_guard.take();
        let wire_result = runtime.provider_result(wire_result)?;
        let mut transition = EffectApplier::new(&runtime.resources)
            .lower_effects(effects, clock::now_millis())
            .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
        transition.blobs.extend(pending_blobs);
        Ok((
            wire_result,
            Self {
                runtime,
                span,
                captured_epoch,
                transition,
            },
        ))
    }

    fn transition_mut(&mut self) -> &mut crate::cache::ProjectionTransition {
        &mut self.transition
    }

    fn transition(&self) -> &crate::cache::ProjectionTransition {
        &self.transition
    }

    fn merge(&mut self, addition: crate::cache::ProjectionTransition) {
        merge_transition(&mut self.transition, addition);
    }

    fn finish(
        self,
        operation_id: u64,
        outcome: InspectorOutcome,
        subtree: Option<u64>,
    ) -> Result<()> {
        self.runtime
            .publish_transition(self.transition, self.captured_epoch)?;
        inspector::record_outcome(&self.span, outcome);
        if let Some(tree) = subtree {
            inspector::record_subtree_handoff(operation_id, tree);
        }
        Ok(())
    }
}

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
            let (wire_result, mut terminal) = TerminalTransaction::prepare(
                self,
                span.clone(),
                captured_epoch,
                blob_guard,
                wire_result,
                &effects,
                |result, effects| {
                    crate::op_validate::validate_lookup(result, effects, |tree| {
                        self.tree_ref(tree).is_some()
                    })
                },
            )?;
            let applier = EffectApplier::new(&self.resources);
            let (result, result_transition) = applier
                .lower_lookup(
                    parent_path,
                    &child_path,
                    wire_result,
                    clock::now_millis(),
                    |tree| self.tree_ref(tree).map(|reference| reference.id),
                )
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            terminal.merge(result_transition);
            let subtree = match result {
                LookupOutcome::Subtree(tree) => Some(tree),
                _ => None,
            };
            terminal.finish(id, InspectorOutcome::Ok, subtree)?;
            Ok(result)
        }
        .instrument(span.clone())
        .await
    }

    pub(crate) async fn run_list_children(
        &self,
        path: &Path,
        cursor: Option<wit_types::Cursor>,
        expected_cursor: Option<crate::view::CachedCursor>,
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
                .list_children(id, path.as_str().to_string(), cursor)
                .await?;
            let (wire_result, mut terminal) = TerminalTransaction::prepare(
                self,
                span.clone(),
                captured_epoch,
                blob_guard,
                wire_result,
                &effects,
                |result, effects| {
                    crate::op_validate::validate_list(result, effects, |tree| {
                        self.tree_ref(tree).is_some()
                    })
                },
            )?;
            let applier = EffectApplier::new(&self.resources);
            let (result, result_transition) = applier
                .lower_list(path, wire_result, expected_cursor, |tree| {
                    self.tree_ref(tree).map(|reference| reference.id)
                })
                .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
            terminal.merge(result_transition);
            let subtree = match result {
                ListOutcome::Subtree(tree) => Some(tree),
                ListOutcome::Entries(_) => None,
            };
            terminal.finish(id, InspectorOutcome::Ok, subtree)?;
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
    ) -> Result<ReadOutcome> {
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
            let (wire_result, mut terminal) = TerminalTransaction::prepare(
                self,
                span.clone(),
                captured_epoch,
                blob_guard,
                wire_result,
                &effects,
                |result, effects| {
                    crate::op_validate::validate_read(result, effects, |tree| {
                        self.tree_ref(tree).is_some()
                    })
                },
            )?;
            let applier = EffectApplier::new(&self.resources);
            let result = match wire_result {
                wit_types::ReadFileOutcome::Found(value) => {
                    let (result, result_transition) = applier
                        .lower_read(
                            path,
                            wit_types::ReadFileOutcome::Found(value),
                            &terminal.transition().objects,
                        )
                        .map_err(|error| EngineError::ProviderProtocol(error.to_string()))?;
                    terminal.merge(result_transition);
                    result
                },
                wit_types::ReadFileOutcome::NotFound(maybe_id) => {
                    terminal
                        .transition_mut()
                        .records
                        .push(crate::cache::RecordWrite {
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
                    terminal
                        .transition_mut()
                        .freshness
                        .push(crate::cache::Freshness {
                            path: path.clone(),
                            expires_at: Some(
                                clock::now_millis().saturating_add(clock::DYNAMIC_TTL_MILLIS),
                            ),
                        });
                    terminal.finish(id, InspectorOutcome::NotFound, None)?;
                    return Err(EngineError::ProviderError(wit_types::ProviderError {
                        kind: wit_types::ErrorKind::NotFound,
                        message: format!("no such file: {path}"),
                        retryable: false,
                        retry_after: None,
                    }));
                },
            };
            terminal.finish(id, InspectorOutcome::Ok, None)?;
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
            let (wire_result, mut terminal) = TerminalTransaction::prepare(
                self,
                span.clone(),
                captured_epoch,
                blob_guard,
                wire_result,
                &effects,
                |result, effects| {
                    crate::op_validate::validate_open(result, effects, |tree| {
                        self.tree_ref(tree).is_some()
                    })
                },
            )?;
            let outcome = OpenOutcome::from_wit(wire_result);
            let meta = crate::view::EntryMeta::file(outcome.attrs.clone());
            terminal.transition_mut().records.extend([
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
            terminal.finish(id, InspectorOutcome::Ok, None)?;
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
            let (wire_result, mut terminal) = TerminalTransaction::prepare(
                self,
                span.clone(),
                captured_epoch,
                blob_guard,
                wire_result,
                &effects,
                |result, effects| {
                    crate::op_validate::validate_chunk(result, effects, length, |tree| {
                        self.tree_ref(tree).is_some()
                    })
                },
            )?;
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
                    terminal.transition_mut().records.extend([
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
            terminal.finish(id, InspectorOutcome::Ok, None)?;
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
            let ((), terminal) = TerminalTransaction::prepare(
                self,
                span.clone(),
                captured_epoch,
                blob_guard,
                wire_result,
                &effects,
                |result, effects| {
                    crate::op_validate::validate_event(result, effects, |tree| {
                        self.tree_ref(tree).is_some()
                    })
                },
            )?;
            terminal.finish(id, InspectorOutcome::Ok, None)?;
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

    fn provider_result<T>(
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
}

fn dirents_path(mutation: &crate::cache::DirentsMutation) -> &Path {
    match mutation {
        crate::cache::DirentsMutation::Replace { path, .. }
        | crate::cache::DirentsMutation::MergeHints { path, .. }
        | crate::cache::DirentsMutation::AppendPage { path, .. } => path,
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
        .position(|existing| dirents_path(existing) == dirents_path(&addition))
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
                exhaustive,
            },
            crate::cache::DirentsMutation::MergeHints {
                entries: additions,
                exhaustive: addition_exhaustive,
                ..
            },
        ) => {
            merge_hint_entries(&mut entries, additions, true);
            crate::cache::DirentsMutation::MergeHints {
                path,
                entries,
                exhaustive: exhaustive || addition_exhaustive,
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
            merge_hint_entries(&mut value.entries, entries, false);
            crate::cache::DirentsMutation::Replace { path, value }
        },
        (
            crate::cache::DirentsMutation::MergeHints { entries, .. },
            crate::cache::DirentsMutation::AppendPage {
                path,
                expected_cursor,
                entries: mut page_entries,
                next_cursor,
                exhaustive,
            },
        )
        | (
            crate::cache::DirentsMutation::AppendPage {
                path,
                expected_cursor,
                entries: mut page_entries,
                next_cursor,
                exhaustive,
            },
            crate::cache::DirentsMutation::MergeHints { entries, .. },
        ) => {
            merge_hint_entries(&mut page_entries, entries, false);
            crate::cache::DirentsMutation::AppendPage {
                path,
                expected_cursor,
                entries: page_entries,
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

fn inspect_result<T>(
    span: &tracing::Span,
    result: &std::result::Result<T, wit_types::ProviderError>,
) {
    if let Err(error) = result {
        inspector::record_outcome(span, inspector::outcome_for_provider_error(error));
    }
}
