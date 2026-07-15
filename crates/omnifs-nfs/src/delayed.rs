//! NFS-local proactive deferral of provider-backed directory listings.
//!
//! `Tree` computes the truthful projection result and may block on cold provider
//! work for as long as it takes. The NFS frontend decides how long an individual
//! RPC handler may wait for that truth before replying `NFS4ERR_DELAY` and
//! letting the client retry. That wait budget is frontend policy; `Tree`
//! deliberately does not own it.
//!
//! Concurrent RPC dispatch already keeps one slow op from head-of-line blocking
//! other calls on the same connection. Proactive deferral is about not holding a
//! single `READDIR` reply past the inline budget so the client stays responsive.
//!
//! [`PendingListings`] runs each listing once per directory as a detached task,
//! lets a caller wait up to a small budget, and reports [`PendingOutcome::Pending`]
//! past it. The task is never cancelled, so a slow listing runs to completion
//! and writes its dirents into the namespace cache; the client's retry then
//! re-resolves and hits that warm cache.
//!
//! This convergence holds only on the success path, which the namespace caches.
//! An errored listing is not cached, so a slow, persistently failing listing
//! re-runs on every retry until it succeeds or the upstream error maps to a
//! terminal status. That is why this table backs `READDIR` and not `LOOKUP`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use omnifs_engine::namespace::{DirEntry as NsDirEntry, NodeId};
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::export::Status;

/// The already-mapped terminal result of a deferred listing: the fully-drained
/// namespace snapshot for one directory. The adapter does the `NsError` to
/// `Status` conversion before it reaches this table, so this module never
/// touches protocol state.
pub(crate) type ListResult = Result<Vec<NsDirEntry>, Status>;

/// Outcome of waiting for one directory listing within a caller's budget.
pub(crate) enum PendingOutcome {
    Ready(Arc<ListResult>),
    Pending,
}

type SlotSender = watch::Sender<Option<Arc<ListResult>>>;
type Slots = HashMap<NodeId, SlotSender>;

/// Per-directory detached work with a per-caller wait budget for NFS listings.
///
/// The table retains only active slots. A completed result is delivered to the
/// callers already waiting on that slot and then forgotten, so later retries
/// return to the namespace rather than being served by an NFS-side cache.
pub(crate) struct PendingListings {
    runtime: Handle,
    slots: Arc<Mutex<Slots>>,
}

impl PendingListings {
    pub(crate) fn new(runtime: Handle) -> Self {
        Self {
            runtime,
            slots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn resolve<F, Fut>(&self, node: NodeId, budget: Duration, make: F) -> PendingOutcome
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ListResult> + Send + 'static,
    {
        let (receiver, leader) = {
            let mut slots = lock_slots(&self.slots);
            if let Some(sender) = slots.get(&node) {
                (sender.subscribe(), false)
            } else {
                let (sender, receiver) = watch::channel::<Option<Arc<ListResult>>>(None);
                slots.insert(node, sender.clone());
                (receiver, true)
            }
        };

        if leader {
            let slots = Arc::clone(&self.slots);
            let runtime = self.runtime.clone();
            runtime.spawn(async move {
                let result = Arc::new(make().await);
                let mut slots = lock_slots(&slots);
                if let Some(sender) = slots.get(&node) {
                    let _ = sender.send(Some(result));
                }
                slots.remove(&node);
            });
        }

        self.runtime.block_on(wait_for(receiver, budget))
    }
}

fn lock_slots(slots: &Mutex<Slots>) -> MutexGuard<'_, Slots> {
    slots.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn wait_for(
    mut receiver: watch::Receiver<Option<Arc<ListResult>>>,
    budget: Duration,
) -> PendingOutcome {
    if let Some(result) = receiver.borrow().clone() {
        return PendingOutcome::Ready(result);
    }

    match tokio::time::timeout(budget, receiver.changed()).await {
        Ok(Ok(())) => receiver
            .borrow()
            .clone()
            .map_or(PendingOutcome::Pending, PendingOutcome::Ready),
        Ok(Err(_)) | Err(_) => PendingOutcome::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::{PendingListings, PendingOutcome};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use omnifs_engine::namespace::NodeId;

    fn multi_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("multi-thread runtime")
    }

    #[test]
    fn slow_leader_is_shared_after_first_caller_returns_pending() {
        let runtime = multi_thread_runtime();
        let listings = Arc::new(PendingListings::new(runtime.handle().clone()));
        let calls = Arc::new(AtomicUsize::new(0));
        let node = NodeId(7);

        assert!(matches!(
            listings.resolve(node, Duration::from_millis(40), {
                let calls = Arc::clone(&calls);
                move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Ok(Vec::new())
                }
            }),
            PendingOutcome::Pending
        ));

        match listings.resolve(node, Duration::from_secs(2), {
            let calls = Arc::clone(&calls);
            move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }
        }) {
            PendingOutcome::Ready(result) => assert!(result.is_ok()),
            PendingOutcome::Pending => panic!("expected the shared listing to finish"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn completed_slot_is_forgotten_for_later_retry() {
        let runtime = multi_thread_runtime();
        let listings = PendingListings::new(runtime.handle().clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let node = NodeId(9);

        match listings.resolve(node, Duration::from_secs(2), {
            let calls = Arc::clone(&calls);
            move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }
        }) {
            PendingOutcome::Ready(result) => assert!(result.is_ok()),
            PendingOutcome::Pending => panic!("expected the listing to finish"),
        }

        match listings.resolve(node, Duration::from_secs(2), {
            let calls = Arc::clone(&calls);
            move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }
        }) {
            PendingOutcome::Ready(result) => assert!(result.is_ok()),
            PendingOutcome::Pending => panic!("expected the retried listing to finish"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
