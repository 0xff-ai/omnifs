//! Internal machinery: the per-operation future table behind suspend/resume.
//!
//! Providers never call this directly. The `#[omnifs_sdk::provider]` macro
//! owns one `AsyncRuntime` per provider in a thread-local and drives it from
//! the WIT entry points: each browse export starts a handler future here,
//! and the `continuation` export's `resume` re-enters it with the host's
//! callout results.
//!
//! Lifecycle per operation id: [`AsyncRuntime::start`] polls the future
//! once. `Pending` with yielded callouts parks the future and its [`Cx`]
//! under the id and returns a suspend step carrying the batch; `Ready`
//! returns the terminal. On [`AsyncRuntime::resume`] the host's results,
//! positionally ordered against the yielded batch, are pushed FIFO into the
//! `Cx` delivery queue before the future is re-polled; the parked entry is
//! removed first, so a second resume for the same id is an error, not a
//! re-poll. Dropping a parked future ([`AsyncRuntime::cancel`] or
//! [`AsyncRuntime::clear`]) is cancellation.
//!
//! Two states are rejected as internal errors instead of being parked,
//! because each would otherwise wedge the host's view of the operation:
//! `Pending` with no yielded callouts (a stalled future nothing will ever
//! wake) and `Ready` with callouts still queued (a terminal racing staged
//! work).

use crate::cx::Cx;
use crate::error::ProviderError;
use crate::hashbrown::HashMap;
use crate::prelude::{CalloutResults, OpResult, ProviderReturn, ProviderStep};
use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

type HandlerFuture = Pin<Box<dyn Future<Output = ProviderReturn>>>;

/// Table of suspended handler futures keyed by operation id. Single-threaded
/// by construction (futures are `!Send`, guests run one call at a time), so
/// a `RefCell` suffices. See the module docs for the lifecycle contract.
#[doc(hidden)]
pub struct AsyncRuntime<S> {
    pending: RefCell<HashMap<u64, (HandlerFuture, Cx<S>)>>,
}

impl<S> Default for AsyncRuntime<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> AsyncRuntime<S> {
    pub fn new() -> Self {
        Self {
            pending: RefCell::new(HashMap::new()),
        }
    }

    /// Drop every parked future. Called on provider shutdown so suspended
    /// operations do not outlive the instance.
    pub fn clear(&self) {
        self.pending.borrow_mut().clear();
    }

    /// Drop one parked future; dropping is the cancellation mechanism. A
    /// later `resume` for the id finds nothing and reports the missing
    /// operation.
    pub fn cancel(&self, id: u64) {
        self.pending.borrow_mut().remove(&id);
    }
}

impl<S: 'static> AsyncRuntime<S> {
    /// Begin driving a handler future for operation `id`. Returns either a
    /// terminal step or a suspension carrying the first callout batch.
    pub fn start(&self, id: u64, cx: Cx<S>, future: HandlerFuture) -> ProviderStep {
        self.poll(id, future, cx)
    }

    /// Re-enter a parked operation with the host's callout results. The
    /// results must be positionally ordered against the batch the operation
    /// last yielded; they are queued FIFO before the re-poll so awaiting
    /// futures consume them in yield order. Returns `None` when no future is
    /// parked under `id` (never started, already finished, or cancelled);
    /// an empty result list is rejected because a suspension always awaits
    /// at least one result.
    pub fn resume(&self, id: u64, outcomes: CalloutResults) -> Option<ProviderStep> {
        let (future, cx) = self.pending.borrow_mut().remove(&id)?;
        if outcomes.is_empty() {
            return Some(ProviderStep::returned(
                ProviderError::internal("expected at least one callout result").into(),
            ));
        }
        for outcome in outcomes {
            cx.push_delivered(outcome);
        }
        Some(self.poll(id, future, cx))
    }

    fn poll(&self, id: u64, mut future: HandlerFuture, cx: Cx<S>) -> ProviderStep {
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(response) => {
                let callouts = cx.take_yielded_callouts();
                if !callouts.is_empty() {
                    return ProviderStep::returned(ProviderReturn::terminal(OpResult::from(
                        ProviderError::internal("future returned while yielding callouts"),
                    )));
                }
                ProviderStep::returned(response)
            },
            Poll::Pending => {
                let callouts = cx.take_yielded_callouts();
                if callouts.is_empty() {
                    // Stalled guest future with no staged work: cancel and
                    // surface an internal error rather than wedging the host.
                    return ProviderStep::returned(ProviderReturn::terminal(OpResult::from(
                        ProviderError::internal("future polled Pending without yielding callouts"),
                    )));
                }
                self.pending.borrow_mut().insert(id, (future, cx));
                ProviderStep::suspend(callouts)
            },
        }
    }
}
