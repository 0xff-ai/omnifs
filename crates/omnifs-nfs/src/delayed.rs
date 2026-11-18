//! NFS-local proactive deferral of provider-backed directory listings.
//!
//! `EngineNamespace` computes the truthful projection result and may block on cold provider
//! work for as long as it takes. The NFS filesystem decides how long an individual
//! RPC handler may wait for that truth before replying `NFS4ERR_DELAY` and
//! letting the client retry. That wait budget is filesystem policy; `EngineNamespace`
//! deliberately does not own it.
//!
//! Concurrent RPC dispatch already keeps one slow op from head-of-line blocking
//! other calls on the same connection. Proactive deferral is about not holding a
//! single `READDIR` reply past the inline budget so the client stays responsive.
//!
//! [`PendingListings`] runs each listing once per directory as a detached task,
//! lets a caller wait up to a small budget, and reports [`PendingOutcome::Pending`]
//! past it. The task is never cancelled, so a slow listing runs to completion,
//! and a result that no caller took waits in its slot for the client's retry,
//! which takes it: a retry that re-resolved would ask the namespace again,
//! which asks the provider again for a listing past its deadline by then, past
//! the budget again at every retry.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use omnifs_core::path::Path;
use omnifs_vfs::DirEntry as NsDirEntry;
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

/// The slot of one directory: the serial of its listing task, which no other
/// slot has, and the result of the task once it completes.
struct Slot {
    serial: u64,
    sender: watch::Sender<Option<Arc<ListResult>>>,
}

struct ListingState {
    next_serial: u64,
    slots: HashMap<Path, Slot>,
}

/// Per-directory detached work with a per-caller wait budget for NFS listings.
///
/// A completed result goes to the callers waiting on its slot, and a result
/// that no caller took waits there for the next caller of the directory. The
/// caller that takes a result removes its slot, so the next listing of the
/// directory returns to the namespace.
pub(crate) struct PendingListings {
    runtime: Handle,
    state: Arc<Mutex<ListingState>>,
}

impl PendingListings {
    pub(crate) fn new(runtime: Handle) -> Self {
        Self {
            runtime,
            state: Arc::new(Mutex::new(ListingState {
                next_serial: 0,
                slots: HashMap::new(),
            })),
        }
    }

    pub(crate) fn resolve<F, Fut>(&self, path: &Path, budget: Duration, make: F) -> PendingOutcome
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ListResult> + Send + 'static,
    {
        let (receiver, leader, serial) = {
            let mut state = lock_state(&self.state);
            if let Some(slot) = state.slots.get(path) {
                (slot.sender.subscribe(), false, slot.serial)
            } else {
                let serial = state.next_serial;
                state.next_serial = serial.wrapping_add(1);
                let (sender, receiver) = watch::channel::<Option<Arc<ListResult>>>(None);
                state.slots.insert(path.clone(), Slot { serial, sender });
                (receiver, true, serial)
            }
        };

        if leader {
            let state = Arc::clone(&self.state);
            let path = path.clone();
            self.runtime.spawn(async move {
                let result = Arc::new(make().await);
                if let Some(slot) = lock_state(&state).slots.get(&path)
                    && slot.serial == serial
                {
                    slot.sender.send_replace(Some(result));
                }
            });
        }

        let outcome = self.runtime.block_on(wait_for(receiver, budget));
        if matches!(outcome, PendingOutcome::Ready(_)) {
            let mut state = lock_state(&self.state);
            if state
                .slots
                .get(path)
                .is_some_and(|slot| slot.serial == serial)
            {
                state.slots.remove(path);
            }
        }
        outcome
    }

    /// Drop the slots in `path`'s subtree, pending or with a result that waits;
    /// a detached completion publishes only into the slot of its own serial.
    pub(crate) fn reset(&self, path: &Path) {
        lock_state(&self.state)
            .slots
            .retain(|slot_path, _| !slot_path.has_prefix(path));
    }
}

fn lock_state(state: &Mutex<ListingState>) -> MutexGuard<'_, ListingState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
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
    use super::{ListResult, PendingListings, PendingOutcome};
    use crate::export::Status;
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use omnifs_core::path::Path;

    fn path(value: &str) -> Path {
        Path::parse(value).expect("test path")
    }

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
        let node = path("/test/items");

        assert!(matches!(
            listings.resolve(&node, Duration::from_millis(40), {
                let calls = Arc::clone(&calls);
                move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    Err(Status::Io)
                }
            }),
            PendingOutcome::Pending
        ));

        listings.reset(&node);

        assert!(matches!(
            listings.resolve(&node, Duration::from_millis(40), {
                let calls = Arc::clone(&calls);
                move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok(Vec::new())
                }
            }),
            PendingOutcome::Pending
        ));

        match listings.resolve(&node, Duration::from_secs(2), {
            let calls = Arc::clone(&calls);
            move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(Status::Io)
            }
        }) {
            PendingOutcome::Ready(result) => assert!(result.is_ok()),
            PendingOutcome::Pending => panic!("expected the shared listing to finish"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn completed_slot_is_forgotten_for_later_retry() {
        let runtime = multi_thread_runtime();
        let listings = PendingListings::new(runtime.handle().clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let node = path("/test/items");

        match listings.resolve(&node, Duration::from_secs(2), {
            let calls = Arc::clone(&calls);
            move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }
        }) {
            PendingOutcome::Ready(result) => assert!(result.is_ok()),
            PendingOutcome::Pending => panic!("expected the listing to finish"),
        }

        match listings.resolve(&node, Duration::from_secs(2), {
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

    /// A listing of 100 ms, past the budget of its callers like one that asks
    /// its provider again past its deadline.
    fn slow_listing(
        calls: &Arc<AtomicUsize>,
    ) -> impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = ListResult> + Send>> + use<> {
        let calls = Arc::clone(calls);
        move || {
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(Vec::new())
            })
        }
    }

    #[test]
    fn a_result_no_caller_took_waits_for_the_retry() {
        let runtime = multi_thread_runtime();
        let listings = PendingListings::new(runtime.handle().clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let node = path("/test/items");
        let budget = Duration::from_millis(20);

        assert!(matches!(
            listings.resolve(&node, budget, slow_listing(&calls)),
            PendingOutcome::Pending
        ));
        std::thread::sleep(Duration::from_millis(300));
        match listings.resolve(&node, budget, slow_listing(&calls)) {
            PendingOutcome::Ready(result) => assert!(result.is_ok()),
            PendingOutcome::Pending => panic!("expected the retry to take the deferred result"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        match listings.resolve(&node, Duration::from_secs(2), slow_listing(&calls)) {
            PendingOutcome::Ready(result) => assert!(result.is_ok()),
            PendingOutcome::Pending => panic!("expected a new listing to finish"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_reset_of_another_directory_keeps_a_result_that_waits() {
        let runtime = multi_thread_runtime();
        let listings = PendingListings::new(runtime.handle().clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let node = path("/test/items");
        let budget = Duration::from_millis(20);

        assert!(matches!(
            listings.resolve(&node, budget, slow_listing(&calls)),
            PendingOutcome::Pending
        ));
        listings.reset(&path("/test/other"));
        std::thread::sleep(Duration::from_millis(300));
        match listings.resolve(&node, budget, slow_listing(&calls)) {
            PendingOutcome::Ready(result) => assert!(result.is_ok()),
            PendingOutcome::Pending => panic!("expected the retry to take the deferred result"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
