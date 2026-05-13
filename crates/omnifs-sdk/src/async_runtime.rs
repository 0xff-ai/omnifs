use crate::cx::Cx;
use crate::error::ProviderError;
use crate::hashbrown::HashMap;
use crate::prelude::{CalloutResults, OpResult, ProviderReturn, ProviderStep};
use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

type HandlerFuture = Pin<Box<dyn Future<Output = ProviderReturn>>>;

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

    pub fn clear(&self) {
        self.pending.borrow_mut().clear();
    }

    pub fn cancel(&self, id: u64) {
        self.pending.borrow_mut().remove(&id);
    }
}

impl<S: 'static> AsyncRuntime<S> {
    pub fn start(&self, id: u64, cx: Cx<S>, future: HandlerFuture) -> ProviderStep {
        self.poll(id, future, cx)
    }

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
