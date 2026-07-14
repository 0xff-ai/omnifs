//! Handler execution context and concurrent callout polling.
//!
//! [`Cx<State>`](Cx) is what an async handler holds: typed provider state
//! plus op-level metadata such as the host-assigned operation id and cached
//! validator. Awaiting a callout future awaits a WIT async host import; the
//! component runtime suspends the operation while the host executes the effect.
//!
//! [`join_all`] polls sibling callout futures in one operation so the async
//! component runtime can keep several host imports in flight at once.

use crate::git;
use crate::http;
use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;

/// Execution context for async provider handlers.
///
/// `Cx` separates op-level metadata ([`CxShared`]: the id and host-pushed
/// validator) from the typed provider `State`. The shared part is
/// reference-counted so cloned contexts and state-erased range readers keep
/// the same operation identity.
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

/// Op-level metadata, independent of the provider state type so a state-erased
/// [`Cx`] shares the same operation identity (see [`Cx::erase_state`]).
struct CxShared {
    id: u64,
    /// The host-pushed validator for this path's anchor, if held. Set by host
    /// glue via [`Cx::with_version`]; read by handlers through [`Cx::version`].
    version: Option<crate::file_attrs::VersionToken>,
}

impl<S> Cx<S> {
    /// Create a new context for the given operation id and state handle.
    pub fn new(id: u64, state: Rc<RefCell<S>>) -> Self {
        Self {
            shared: Rc::new(CxShared { id, version: None }),
            state,
        }
    }

    /// Attach the host-pushed validator for this anchor. Called by host glue
    /// before a handler or `Object::load` runs; the validator is read back via
    /// [`Self::version`]. Rebuilds the shared cell because the validator is
    /// fixed for the lifetime of a single operation.
    #[doc(hidden)]
    #[must_use]
    pub fn with_version(self, version: Option<crate::file_attrs::VersionToken>) -> Self {
        Self {
            shared: Rc::new(CxShared {
                id: self.shared.id,
                version,
            }),
            state: Rc::clone(&self.state),
        }
    }

    /// A state-erased view sharing this operation's identity. Used by ranged
    /// readers, whose handle type is state-erased but whose callouts still run
    /// under the operation the runtime is driving.
    #[doc(hidden)]
    pub fn erase_state(&self) -> Cx<()> {
        Cx {
            shared: Rc::clone(&self.shared),
            state: Rc::new(RefCell::new(())),
        }
    }

    /// The host-pushed validator for this path's anchor, if held. A handler
    /// maps it to `If-None-Match` through
    /// [`crate::endpoint::RequestBuilder::maybe_if_none_match`].
    pub fn version(&self) -> Option<&crate::file_attrs::VersionToken> {
        self.shared.version.as_ref()
    }

    #[doc(hidden)]
    pub fn id(&self) -> u64 {
        self.shared.id
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
}

/// Run a collection of callout futures concurrently and collect their
/// outputs in input order. Polling every child before returning `Pending`
/// starts every generated async host import the child reaches, so the host can
/// run the corresponding HTTP, git, or blob work concurrently.
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
/// what starts sibling host imports promptly; children finish independently and
/// the output preserves input positions.
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
    use std::future::ready;

    #[test]
    fn join_all_preserves_input_order_for_ready_children() {
        let mut combined = Box::pin(join_all([ready("a"), ready("b"), ready("c")]));
        let waker = std::task::Waker::noop();
        let mut ctx = Context::from_waker(waker);

        let Poll::Ready(results) = combined.as_mut().poll(&mut ctx) else {
            panic!("expected ready children to complete");
        };

        assert_eq!(results, ["a", "b", "c"]);
    }
}
