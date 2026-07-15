//! Typed provider operation execution and terminal effect publication.

use crate::Runtime;
use crate::clock;
use crate::inspector;
use crate::runtime::{EngineError, Result};
use omnifs_api::events::InspectorOutcome;
use omnifs_core::path::{Path, Segment};
use omnifs_wit::provider::types as wit_types;
use tracing::Instrument;

impl Runtime {
    pub(crate) async fn run_lookup_child(
        &self,
        parent_path: &Path,
        name: &Segment,
    ) -> Result<wit_types::LookupChildResult> {
        let id = self.next_operation_id();
        let joined_path = parent_path.join_segment(name);
        let span = inspector::provider_span(
            id,
            &self.mount_name,
            &self.provider_name,
            "lookup_child",
            joined_path.as_str(),
        );
        async {
            let op_gen = self.resources.current_generation();
            let (result, effects) = self
                .instance
                .lookup_child(
                    id,
                    parent_path.as_str().to_string(),
                    name.as_str().to_string(),
                )
                .await?;
            crate::op_validate::validate_lookup(&result, &effects, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &result);
            let result = self.provider_result(result)?;
            self.publish_effects(&effects, op_gen)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            if let wit_types::LookupChildResult::Subtree(tree) = &result {
                inspector::record_subtree_handoff(id, *tree);
            }
            Ok(result)
        }
        .instrument(span.clone())
        .await
    }

    pub(crate) async fn run_list_children(
        &self,
        path: &Path,
        cached_validator: Option<String>,
        cursor: Option<wit_types::Cursor>,
    ) -> Result<wit_types::ListChildrenResult> {
        let id = self.next_operation_id();
        let span = inspector::provider_span(
            id,
            &self.mount_name,
            &self.provider_name,
            "list_children",
            path.as_str(),
        );
        async {
            let op_gen = self.resources.current_generation();
            let (result, effects) = self
                .instance
                .list_children(id, path.as_str().to_string(), cached_validator, cursor)
                .await?;
            crate::op_validate::validate_list(&result, &effects, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &result);
            let result = self.provider_result(result)?;
            self.publish_effects(&effects, op_gen)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            if let wit_types::ListChildrenResult::Subtree(tree) = &result {
                inspector::record_subtree_handoff(id, *tree);
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
    ) -> Result<wit_types::ReadFileOutcome> {
        let id = self.next_operation_id();
        let span = inspector::provider_span(
            id,
            &self.mount_name,
            &self.provider_name,
            "read_file",
            path.as_str(),
        );
        async {
            let op_gen = self.resources.current_generation();
            let (result, effects) = self
                .instance
                .read_file(
                    id,
                    path.as_str().to_string(),
                    content_type,
                    cached_canonical,
                )
                .await?;
            crate::op_validate::validate_read(&result, &effects, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &result);
            let result = self.provider_result(result)?;
            self.publish_effects(&effects, op_gen)?;
            self.store_read_not_found_negative(path, &result, op_gen)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            Ok(result)
        }
        .instrument(span.clone())
        .await
    }

    pub(crate) async fn run_open_file(&self, path: &Path) -> Result<wit_types::OpenFileResult> {
        let id = self.next_operation_id();
        let span = inspector::provider_span(
            id,
            &self.mount_name,
            &self.provider_name,
            "open_file",
            path.as_str(),
        );
        async {
            let op_gen = self.resources.current_generation();
            let (result, effects) = self
                .instance
                .open_file(id, path.as_str().to_string())
                .await?;
            crate::op_validate::validate_open(&result, &effects, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &result);
            let result = self.provider_result(result)?;
            self.publish_effects(&effects, op_gen)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            Ok(result)
        }
        .instrument(span.clone())
        .await
    }

    pub(crate) async fn run_read_chunk(
        &self,
        handle: u64,
        offset: u64,
        length: u32,
    ) -> Result<wit_types::ReadChunkResult> {
        let id = self.next_operation_id();
        let span =
            inspector::provider_span(id, &self.mount_name, &self.provider_name, "read_chunk", "");
        async {
            let op_gen = self.resources.current_generation();
            let (result, effects) = self.instance.read_chunk(id, handle, offset, length).await?;
            crate::op_validate::validate_chunk(&result, &effects, length, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &result);
            let result = self.provider_result(result)?;
            self.publish_effects(&effects, op_gen)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            Ok(result)
        }
        .instrument(span.clone())
        .await
    }

    pub(crate) async fn run_event(&self, event: wit_types::ProviderEvent) -> Result<()> {
        let id = self.next_operation_id();
        let span =
            inspector::provider_span(id, &self.mount_name, &self.provider_name, "on_event", "");
        async {
            let op_gen = self.resources.current_generation();
            let (result, effects) = self.instance.on_event(id, event).await?;
            crate::op_validate::validate_event(&result, &effects, |tree| {
                self.tree_ref(tree).is_some()
            })
            .map_err(EngineError::ProviderProtocol)?;
            inspect_result(&span, &result);
            self.provider_result(result)?;
            self.publish_effects(&effects, op_gen)?;
            inspector::record_outcome(&span, InspectorOutcome::Ok);
            Ok(())
        }
        .instrument(span.clone())
        .await
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

    fn store_read_not_found_negative(
        &self,
        path: &Path,
        result: &wit_types::ReadFileOutcome,
        op_gen: u64,
    ) -> Result<()> {
        if let wit_types::ReadFileOutcome::NotFound(maybe_id) = result {
            self.apply_not_found_negative(path, maybe_id.as_ref(), op_gen, clock::now_millis())?;
        }
        Ok(())
    }
}

fn inspect_result<T>(
    span: &tracing::Span,
    result: &std::result::Result<T, wit_types::ProviderError>,
) {
    match result {
        Ok(_) => {},
        Err(error) => inspector::record_outcome(span, inspector::outcome_for_provider_error(error)),
    }
}
