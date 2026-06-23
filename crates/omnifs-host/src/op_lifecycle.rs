//! Op execution loop: start → suspend/resume → apply effects → return.
//!
//! Everything from the first `instance.start_op` call to the final
//! `Materializer::apply` lives here so a read-file round trip is traceable
//! through one seam. `Runtime` retains engine/instance/mount lifecycle and
//! delegates here for all op execution.

use crate::Runtime;
use crate::clock;
use crate::inspector::{self, InspectorProviderOp, WitProviderErrorView};
use crate::op::Op;
use crate::runtime::{Error, Result};
use omnifs_inspector::{InspectorOutcome, OutcomeFields, TraceId};
use omnifs_wit::provider::types as wit_types;

impl Runtime {
    pub(crate) async fn run_op(
        &self,
        op: Op,
        fuse_trace: Option<TraceId>,
    ) -> Result<wit_types::OpResult> {
        // The generation captured here fences any `canonical-write` this op
        // emits: a write is rejected if the anchor was invalidated after the
        // op began (ADR-0001 §4.1).
        let op_gen = self.cache.current_generation();
        let id = self.next_operation_id();
        let trace_id = fuse_trace.or_else(inspector::current_trace_id);
        let live_op = trace_id.and_then(|t| {
            InspectorProviderOp::begin(&op, id, &self.mount_name, &self.provider_name, t)
        });
        let mut resume_round = 0u32;
        let mut step = self.instance.start_op(&op, id)?;
        let result = loop {
            match step {
                wit_types::ProviderStep::Returned(ret) => {
                    let handoff_start = std::time::Instant::now();
                    let outcome = self.finish_provider_return(&op, ret, op_gen);
                    // Emit subtree.start/end when the provider handed
                    // off a tree-ref. Done here, after finish handles
                    // the validation + effect-apply, so the elapsed
                    // reflects the resolution work.
                    if let (Some(trace), Ok(op_result)) = (trace_id, outcome.as_ref())
                        && let Some(tree_ref) = inspector::subtree_tree_ref(op_result)
                        && let Some(sink) = self.inspector.as_ref()
                    {
                        sink.emit_subtree_handoff(trace, id, tree_ref, handoff_start.elapsed());
                    }
                    break outcome;
                },
                wit_types::ProviderStep::Suspended(callouts) => {
                    if callouts.is_empty() {
                        break Err(Error::ProviderProtocol(
                            "provider suspended with no callouts".to_string(),
                        ));
                    }
                    if let Some(live) = &live_op {
                        live.suspend(callouts.len());
                    }
                    let results = self.dispatch_callouts(id, &callouts).await;
                    if let Some(live) = &live_op {
                        live.resume(resume_round, results.len());
                    }
                    resume_round += 1;
                    step = self.instance.resume(id, results)?;
                },
            }
        };
        if let Some(live) = live_op {
            let outcome = match &result {
                Ok(_) => OutcomeFields::ok(),
                Err(Error::ProviderError(error)) => {
                    OutcomeFields::with_outcome(WitProviderErrorView(error).outcome())
                },
                Err(_) => OutcomeFields::with_outcome(InspectorOutcome::Internal),
            };
            live.finish(outcome);
        }
        if let Ok(result) = &result {
            self.note_returned_result(result);
        }
        result
    }

    pub(crate) fn finish_provider_return(
        &self,
        op: &Op,
        ret: wit_types::ProviderReturn,
        op_gen: u64,
    ) -> Result<wit_types::OpResult> {
        crate::op_validate::validate_return(op, &ret, |tree| self.resolve_tree_ref(tree).is_some())
            .map_err(Error::ProviderProtocol)?;
        let now = clock::now_millis();
        let (prefixes, paths) =
            crate::materialize::Materializer::new(&self.cache).apply(&ret.effects, op_gen, now);
        self.record_view_invalidations(prefixes, paths);
        self.store_read_not_found_negative(op, &ret.result, op_gen, now);
        Ok(ret.result)
    }

    fn store_read_not_found_negative(
        &self,
        op: &Op,
        result: &wit_types::OpResult,
        op_gen: u64,
        now_millis: u64,
    ) {
        if let (
            Op::ReadFile { path, .. },
            wit_types::OpResult::ReadFile(wit_types::ReadFileOutcome::NotFound(maybe_id)),
        ) = (op, result)
        {
            self.apply_not_found_negative(path, maybe_id.as_ref(), op_gen, now_millis);
        }
    }

    pub(crate) fn note_returned_result(&self, result: &wit_types::OpResult) {
        if let wit_types::OpResult::Error(e) = result
            && e.kind == wit_types::ErrorKind::RateLimited
        {
            self.note_rate_limited(
                e.retry_after
                    .map(|s| std::time::Duration::from_secs(u64::from(s))),
            );
        }
    }
}
