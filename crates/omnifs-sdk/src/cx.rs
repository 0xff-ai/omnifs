//! Handler execution context and callout batching.
//!
//! [`Cx<State>`](Cx) is what an async handler holds: typed provider state
//! plus the operation's callout machinery. Awaiting a callout future pushes
//! the callout onto the yield queue and suspends; the runtime drains the
//! queue after every poll and hands the batch to the host, which runs it
//! concurrently and resumes the operation with results in batch order.
//! Every callout is strictly request/response: each one yielded expects
//! exactly one typed result back, matched positionally, not by id.
//!
//! [`join_all`] exploits that protocol to fan out N callout futures in one
//! suspension round instead of N sequential round trips. The positional
//! matching is also its sharp edge: every child must belong to the same
//! `Cx` and yield exactly one callout per suspension, or sibling results
//! silently misalign (see [`join_all`]).

use crate::archives;
use crate::blob::{BlobId, BlobReader};
use crate::git;
use crate::http;
use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use omnifs_wit::provider::types::{Callout, CalloutResult};
use std::collections::VecDeque;
use std::rc::Rc;

/// Execution context for async provider handlers.
///
/// `Cx` separates the op-level callout machinery ([`CxShared`]: the id, the
/// yield/deliver queues, the host-pushed validator) from the typed provider
/// `State`. The shared part is reference-counted so a state-erased view
/// ([`Cx::erase_state`]) drives callouts on the *same* operation; this is
/// how `Object::load`, which takes `&Cx<()>`, issues fetches through the
/// operation's queue while the router holds `Cx<S>`.
pub struct Cx<S = ()> {
    shared: Rc<CxShared>,
    state: Rc<RefCell<S>>,
}

// Manual `Clone` to avoid the `S: Clone` bound that `#[derive(Clone)]` would
// add. `Cx` clones via `Rc`, so `S` need not be `Clone`.
impl<S> Clone for Cx<S> {
    fn clone(&self) -> Self {
        Self {
            shared: Rc::clone(&self.shared),
            state: Rc::clone(&self.state),
        }
    }
}

/// Op-level callout machinery, independent of the provider state type so a
/// state-erased [`Cx`] shares the same queues (see [`Cx::erase_state`]).
struct CxShared {
    id: u64,
    yielded: RefCell<Vec<Callout>>,
    delivered: RefCell<VecDeque<CalloutResult>>,
    /// The host-pushed validator for this path's anchor, if held (ADR-0001
    /// §5.2). Set by host glue via [`Cx::with_version`]; read by handlers
    /// through [`Cx::version`].
    version: Option<crate::file_attrs::VersionToken>,
}

impl<S> Cx<S> {
    /// Create a new context for the given operation id and state handle.
    pub fn new(id: u64, state: Rc<RefCell<S>>) -> Self {
        Self {
            shared: Rc::new(CxShared {
                id,
                yielded: RefCell::new(Vec::new()),
                delivered: RefCell::new(VecDeque::new()),
                version: None,
            }),
            state,
        }
    }

    /// Attach the host-pushed validator for this anchor. Called by host glue
    /// before a handler or `Object::load` runs; the validator is read back via
    /// [`Self::version`]. Rebuilds the shared cell (fresh queues) because the
    /// validator is fixed for the lifetime of a single operation.
    #[doc(hidden)]
    #[must_use]
    pub fn with_version(self, version: Option<crate::file_attrs::VersionToken>) -> Self {
        Self {
            shared: Rc::new(CxShared {
                id: self.shared.id,
                yielded: RefCell::new(Vec::new()),
                delivered: RefCell::new(VecDeque::new()),
                version,
            }),
            state: Rc::clone(&self.state),
        }
    }

    /// A state-erased view sharing this operation's callout machinery. Used to
    /// call `Object::load(&Cx<()>, ..)` from a state-bearing router: the erased
    /// context issues callouts on the same yield/deliver queue, so object loads
    /// suspend and resume on the operation the runtime is driving.
    #[doc(hidden)]
    pub fn erase_state(&self) -> Cx<()> {
        Cx {
            shared: Rc::clone(&self.shared),
            state: Rc::new(RefCell::new(())),
        }
    }

    /// The host-pushed validator for this path's anchor, if held (ADR-0001
    /// §5.2). A handler maps it to `If-None-Match` through
    /// [`crate::endpoint::RequestBuilder::maybe_if_none_match`].
    pub fn version(&self) -> Option<&crate::file_attrs::VersionToken> {
        self.shared.version.as_ref()
    }

    /// A typed handle to an outbound [`crate::endpoint::Endpoint`] value. Pass
    /// a unit struct for a fixed upstream, or a field-carrying value for a
    /// runtime-resolved base.
    pub fn endpoint<E: crate::endpoint::EndpointHooks>(
        &self,
        endpoint: E,
    ) -> crate::endpoint::EndpointHandle<'_, E, S> {
        crate::endpoint::EndpointHandle::new(self, endpoint)
    }

    /// Read the provider state. The closure scopes the `RefCell` borrow so
    /// it can never be held across an `.await`; do not call [`Self::state`]
    /// or [`Self::state_mut`] reentrantly from inside `f`.
    pub fn state<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        let state = self.state.borrow();
        f(&state)
    }

    /// Mutate the provider state. Same borrow discipline as [`Self::state`]:
    /// the closure scopes the exclusive borrow, and reentrant access from
    /// inside `f` panics.
    pub fn state_mut<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        let mut state = self.state.borrow_mut();
        f(&mut state)
    }

    /// Raw HTTP callout builder against a fully formed URL. Prefer
    /// [`Self::endpoint`] for declared upstream hosts.
    pub fn http(&self) -> http::Builder<'_, S> {
        http::Builder::new(self)
    }

    /// Git callout builder; see [`crate::git`] for the open/clone contract.
    pub fn git(&self) -> git::Builder<'_, S> {
        git::Builder::new(self)
    }

    /// Archive-mount callout builder; see [`crate::archives`].
    pub fn archives(&self) -> archives::Builder<'_, S> {
        archives::Builder::new(self)
    }

    /// Reader for a blob already stored host-side. Each read copies bytes
    /// into guest memory under a host policy cap; see
    /// [`crate::blob::BlobReader`].
    pub fn blob(&self, id: BlobId) -> BlobReader<'_, S> {
        BlobReader::new(self, id)
    }

    /// Drain the callouts queued since the last poll. Exposed for the
    /// WIT-boundary test harness, which drives the suspend/resume loop by
    /// draining yielded callouts and pushing canned results; the host runtime
    /// glue uses the same queue.
    #[doc(hidden)]
    pub fn take_yielded_callouts(&self) -> Vec<Callout> {
        std::mem::take(&mut *self.shared.yielded.borrow_mut())
    }

    /// Deliver a callout result back to a suspended handler. Exposed for the
    /// WIT-boundary test harness (see [`Self::take_yielded_callouts`]).
    #[doc(hidden)]
    pub fn push_delivered(&self, outcome: CalloutResult) {
        self.shared.delivered.borrow_mut().push_back(outcome);
    }

    pub(crate) fn push_yielded(&self, callout: Callout) {
        self.shared.yielded.borrow_mut().push(callout);
    }

    pub(crate) fn pop_delivered(&self) -> Option<CalloutResult> {
        self.shared.delivered.borrow_mut().pop_front()
    }
}

/// Run a collection of callout futures concurrently and collect their
/// outputs in input order. All queued callouts are yielded in a single
/// batch, so the host runs them in parallel and the whole fan-out costs one
/// suspension round instead of one per child; on resume each child consumes
/// its result from the delivery queue in FIFO order.
///
/// Correctness rests on positional alignment, and violations are NOT
/// detected; they surface as siblings receiving each other's results (or a
/// type-mismatch error at best). Every child future MUST:
///
/// - belong to the same `Cx` as its siblings (results are delivered to one
///   operation queue; a child bound to another `Cx` never sees its result
///   and steals nothing from the queue it should have used), and
/// - yield exactly one callout per suspension, the [`crate::http::CalloutFuture`]
///   shape. A child that yields two callouts from one poll, or polls
///   `Pending` without yielding, shifts every later sibling's result.
///
/// Plain SDK callout futures (HTTP sends, blob fetches, git opens, archive
/// opens) and async fns that await them sequentially all satisfy this.
///
/// ```ignore
/// let pages = join_all(
///     (1..=4).map(|p| fetch_page(&cx, p)),
/// )
/// .await; // Vec of per-page results, in page order
/// ```
pub fn join_all<F>(futures: impl IntoIterator<Item = F>) -> JoinAll<F>
where
    F: Future,
{
    let futures: Vec<Option<Pin<Box<F>>>> =
        futures.into_iter().map(|f| Some(Box::pin(f))).collect();
    let len = futures.len();
    JoinAll {
        futures,
        results: (0..len).map(|_| None).collect(),
    }
}

/// Future returned by [`join_all`]. Polls children in index order, which is
/// what keeps yield order (and therefore delivery order) aligned with input
/// order across suspension rounds; children finish independently and the
/// output preserves input positions.
pub struct JoinAll<F: Future> {
    futures: Vec<Option<Pin<Box<F>>>>,
    results: Vec<Option<F::Output>>,
}

// SAFETY: children are stored as `Pin<Box<F>>` (already pinned). The outer
// struct holds no pinned data of its own, so moving `JoinAll` is sound.
impl<F: Future> Unpin for JoinAll<F> {}

impl<F: Future> Future for JoinAll<F> {
    type Output = Vec<F::Output>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut all_ready = true;
        for (i, slot) in this.futures.iter_mut().enumerate() {
            if this.results[i].is_some() {
                continue;
            }
            let Some(future) = slot else { continue };
            match future.as_mut().poll(ctx) {
                Poll::Ready(value) => {
                    this.results[i] = Some(value);
                    *slot = None;
                },
                Poll::Pending => {
                    all_ready = false;
                },
            }
        }
        if !all_ready {
            return Poll::Pending;
        }
        let drained = this
            .results
            .iter_mut()
            .map(|slot| slot.take().expect("all futures ready"))
            .collect();
        Poll::Ready(drained)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_wit::provider::types::{CalloutResult, HttpResponse};
    use std::task::Waker;

    #[test]
    fn join_all_yields_all_callouts_in_a_single_batch() {
        let state = Rc::new(RefCell::new(()));
        let cx = Cx::new(1, state);

        let f1 = cx.http().get("https://a.example/").send();
        let f2 = cx.http().get("https://b.example/").send();
        let f3 = cx.http().get("https://c.example/").send();

        let mut combined = Box::pin(join_all([f1, f2, f3]));
        let waker = Waker::noop();
        let mut ctx = Context::from_waker(waker);
        assert!(matches!(combined.as_mut().poll(&mut ctx), Poll::Pending,));

        let yielded = cx.take_yielded_callouts();
        assert_eq!(yielded.len(), 3);

        for body in ["a", "b", "c"] {
            cx.push_delivered(CalloutResult::HttpResponse(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: body.as_bytes().to_vec(),
            }));
        }

        let Poll::Ready(results) = combined.as_mut().poll(&mut ctx) else {
            panic!("expected ready after delivery");
        };
        let bodies: Vec<Vec<u8>> = results
            .into_iter()
            .map(|r| r.unwrap().into_body())
            .collect();
        assert_eq!(bodies, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }
}
