//! Async executor and context for provider handlers.
//!
//! Provides `Cx<State>` for accessing state and yielding callouts from
//! async handlers. Handlers push callouts onto the yield queue, then
//! `.await` on them (every callout expects a typed result back). The
//! runtime drains the yield queue on every Poll outcome and hands
//! callouts to the host.
//!
//! `join_all` batches N callout futures into a single yield/resume round
//! trip. Every child future must participate in the same `Cx`'s
//! yield/deliver protocol and yield exactly one callout per suspension.

use crate::git;
use crate::http;
use crate::omnifs::provider::types::{Callout, CalloutResult};
use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::collections::VecDeque;
use std::rc::Rc;

/// Execution context for async provider handlers.
#[derive(Clone)]
pub struct Cx<S> {
    inner: Rc<CxInner<S>>,
}

struct CxInner<S> {
    id: u64,
    state: Rc<RefCell<S>>,
    yielded: RefCell<Vec<Callout>>,
    delivered: RefCell<VecDeque<CalloutResult>>,
    active_mount_snapshot: Vec<ActiveMountPaths>,
}

#[derive(Clone)]
struct ActiveMountPaths {
    mount_id: String,
    paths: Vec<String>,
}

impl<S> Cx<S> {
    /// Create a new context for the given operation id and state handle.
    pub fn new(id: u64, state: Rc<RefCell<S>>) -> Self {
        Self::new_with_activity(id, state, Vec::new())
    }

    #[doc(hidden)]
    fn new_with_activity(
        id: u64,
        state: Rc<RefCell<S>>,
        active_mount_snapshot: Vec<ActiveMountPaths>,
    ) -> Self {
        Self {
            inner: Rc::new(CxInner {
                id,
                state,
                yielded: RefCell::new(Vec::new()),
                delivered: RefCell::new(VecDeque::new()),
                active_mount_snapshot,
            }),
        }
    }

    /// Create a new context seeded from a provider event payload.
    pub fn from_event(
        id: u64,
        state: Rc<RefCell<S>>,
        event: &crate::omnifs::provider::types::ProviderEvent,
    ) -> Self {
        let active_mount_snapshot = match event {
            crate::omnifs::provider::types::ProviderEvent::TimerTick(ctx) => ctx
                .active_paths
                .iter()
                .map(|entry| ActiveMountPaths {
                    mount_id: entry.mount_id.clone(),
                    paths: entry.paths.clone(),
                })
                .collect(),
            _ => Vec::new(),
        };
        Self::new_with_activity(id, state, active_mount_snapshot)
    }

    pub fn id(&self) -> u64 {
        self.inner.id
    }

    pub fn state<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        let state = self.inner.state.borrow();
        f(&state)
    }

    pub fn state_mut<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        let mut state = self.inner.state.borrow_mut();
        f(&mut state)
    }

    pub fn http(&self) -> http::Builder<'_, S> {
        http::Builder::new(self)
    }

    pub fn git(&self) -> git::Builder<'_, S> {
        git::Builder::new(self)
    }

    pub(crate) fn take_yielded_callouts(&self) -> Vec<Callout> {
        std::mem::take(&mut *self.inner.yielded.borrow_mut())
    }

    pub(crate) fn push_delivered(&self, outcome: CalloutResult) {
        self.inner.delivered.borrow_mut().push_back(outcome);
    }

    pub(crate) fn push_yielded(&self, callout: Callout) {
        self.inner.yielded.borrow_mut().push(callout);
    }

    pub(crate) fn pop_delivered(&self) -> Option<CalloutResult> {
        self.inner.delivered.borrow_mut().pop_front()
    }

    #[cfg(test)]
    pub(crate) fn take_yielded_callout(&self) -> Option<Callout> {
        self.inner.yielded.borrow_mut().pop()
    }

    pub fn state_handle(&self) -> Rc<RefCell<S>> {
        Rc::clone(&self.inner.state)
    }

    pub fn active_paths<P>(&self, mount_id: &str, parse: impl Fn(&str) -> Option<P>) -> Vec<P> {
        self.inner
            .active_mount_snapshot
            .iter()
            .filter(|entry| entry.mount_id == mount_id)
            .flat_map(|entry| entry.paths.iter())
            .filter_map(|path| parse(path))
            .collect()
    }
}

/// Run a collection of futures concurrently and collect their outputs in
/// order. All queued callouts are yielded in a single batch so the host
/// runs them in parallel; on resume the futures consume their results
/// from the delivery queue in FIFO order.
///
/// Every child future MUST participate in the same `Cx`'s yield/deliver
/// protocol and yield exactly one callout per suspension. Mixing in a
/// future that yields multiple callouts from one poll, or one bound to a
/// different `Cx`, will silently misalign the delivered results across
/// siblings.
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
    use crate::omnifs::provider::types::{
        ActivePathSet, CalloutResult, HttpResponse, ProviderEvent, TimerTickContext,
    };
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

    #[test]
    fn active_paths_are_filtered_by_mount_id_and_parse_success() {
        let state = Rc::new(RefCell::new(()));
        let event = ProviderEvent::TimerTick(TimerTickContext {
            active_paths: vec![
                ActivePathSet {
                    mount_id: "/{owner}/{repo}".to_string(),
                    mount_name: "Repo".to_string(),
                    paths: vec!["/repos/openai/gvfs".to_string(), "/not-a-repo".to_string()],
                },
                ActivePathSet {
                    mount_id: "/other".to_string(),
                    mount_name: "Other".to_string(),
                    paths: vec!["/repos/ignored".to_string()],
                },
            ],
        });

        let cx = Cx::from_event(1, state, &event);
        assert_eq!(
            cx.active_paths("/{owner}/{repo}", |path| {
                path.starts_with("/repos/").then(|| path.to_string())
            }),
            vec!["/repos/openai/gvfs".to_string()]
        );
    }
}
