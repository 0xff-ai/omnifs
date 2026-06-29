//! NFS-local deferral of provider-backed directory listings.
//!
//! `Tree` computes the truthful projection result, blocking on cold provider
//! work (a GitHub fetch, a git clone) for as long as it takes. A loopback NFS
//! frontend cannot hold a `READDIR` reply on that: macOS funnels the mount over
//! one connection and Finder beachballs. Deciding how long to wait for truth,
//! and signalling "retry later" past that budget, is frontend policy `Tree`
//! deliberately does not own.
//!
//! [`DelayedOps`] is that policy. It runs each op once per key as a background
//! task, lets a caller wait a small budget, and reports [`DelayedResult::Pending`]
//! (which the adapter maps to `NFS4ERR_DELAY`) past it. The task is never
//! cancelled, so a slow op runs to completion and writes its result into `Tree`'s
//! cache; the client's `DELAY` retry then re-resolves and hits that warm cache.
//! This holds *only* for operations `Tree` caches (a directory listing writes
//! its dirents record), which is why the table backs `READDIR` and not `LOOKUP`:
//! a cold child lookup is not cached, so deferring it would re-run provider work
//! on every retry. There is no result retention here on purpose: every resolve
//! goes through `Tree`, so caching and invalidation stay `Tree`'s job and a
//! completed op never shadows a later fresh answer. Concurrent and retried
//! callers for the same key share the one task, so a directory is fetched once
//! no matter how many retransmits arrive.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::watch;

/// Outcome of a budgeted [`DelayedOps::resolve`].
pub(crate) enum DelayedResult<V> {
    /// The op finished within budget; carries its terminal value (the op's own
    /// `Ok`/`Err`).
    Ready(V),
    /// Still running past budget; the caller should tell its protocol peer to
    /// retry.
    Pending,
}

type Inflight<K, V> = Arc<Mutex<HashMap<K, watch::Receiver<Option<V>>>>>;

/// Per-key single-flight with a per-caller wait budget for deferrable NFS ops.
///
/// `K` identifies an operation; `V` is the already-mapped terminal result
/// (`Result<_, Status>`), so this stays protocol-agnostic and the adapter keeps
/// all conversion.
pub(crate) struct DelayedOps<K, V> {
    rt: Handle,
    inflight: Inflight<K, V>,
}

impl<K, V> DelayedOps<K, V>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub(crate) fn new(rt: Handle) -> Self {
        Self {
            rt,
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Wait up to `budget` for `key`'s result. The op runs once and is shared
    /// with concurrent/retried callers; exceeding the budget yields `Pending`
    /// while it keeps running to completion (and warms `Tree`'s cache for the
    /// retry).
    pub(crate) fn resolve<F, Fut>(&self, key: K, budget: Duration, make: F) -> DelayedResult<V>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V> + Send + 'static,
    {
        let receiver = {
            let mut inflight = self.inflight.lock().expect("delayed ops lock");
            if let Some(receiver) = inflight.get(&key) {
                receiver.clone()
            } else {
                let (sender, receiver) = watch::channel(None);
                inflight.insert(key.clone(), receiver.clone());
                self.rt
                    .spawn(run_delayed(Arc::clone(&self.inflight), key, make(), sender));
                receiver
            }
        };
        match self
            .rt
            .block_on(async move { tokio::time::timeout(budget, await_value(receiver)).await })
        {
            Ok(Some(value)) => DelayedResult::Ready(value),
            // Budget exceeded (`Err`), or the task died without publishing on a
            // panic (`Ok(None)`): both are "not ready yet" -> the caller retries.
            Ok(None) | Err(_) => DelayedResult::Pending,
        }
    }
}

/// Drive one op to completion and publish its value to current waiters. The
/// `RemoveOnDrop` guard clears the in-flight slot on *every* exit (normal or
/// panic), so a later caller re-resolves through `Tree` instead of attaching to
/// a finished task or, on a panic, a dead slot that would time out forever.
async fn run_delayed<K, V>(
    inflight: Inflight<K, V>,
    key: K,
    future: impl Future<Output = V>,
    sender: watch::Sender<Option<V>>,
) where
    K: Eq + Hash,
    V: Clone,
{
    let _slot = RemoveOnDrop {
        inflight: Arc::clone(&inflight),
        key,
    };
    let value = future.await;
    let _ = sender.send(Some(value));
}

struct RemoveOnDrop<K: Eq + Hash, V> {
    inflight: Inflight<K, V>,
    key: K,
}

impl<K: Eq + Hash, V> Drop for RemoveOnDrop<K, V> {
    fn drop(&mut self) {
        self.inflight
            .lock()
            .expect("delayed ops lock")
            .remove(&self.key);
    }
}

/// Resolve to the task's published value, or `None` if the task dropped its
/// sender without publishing (only on a panic, which the guard has already
/// cleaned up). The channel starts `None`; the task sends `Some(_)` once.
async fn await_value<V: Clone>(mut receiver: watch::Receiver<Option<V>>) -> Option<V> {
    loop {
        if let Some(value) = receiver.borrow_and_update().clone() {
            return Some(value);
        }
        if receiver.changed().await.is_err() {
            return receiver.borrow().clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn multi_thread_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("multi-thread runtime")
    }

    /// A counted op factory: each invocation bumps `calls` and yields `value`
    /// after `delay`.
    fn counted(
        calls: &Arc<AtomicUsize>,
        delay: Duration,
        value: i32,
    ) -> impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = i32> + Send>> {
        let calls = Arc::clone(calls);
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                value
            })
        }
    }

    #[test]
    fn fast_op_returns_ready_within_budget() {
        let rt = multi_thread_rt();
        let ops: DelayedOps<String, i32> = DelayedOps::new(rt.handle().clone());
        let outcome = ops.resolve("k".to_string(), Duration::from_secs(2), || async { 7 });
        assert!(matches!(outcome, DelayedResult::Ready(7)));
    }

    #[test]
    fn slow_op_defers_and_shares_one_running_task() {
        let rt = multi_thread_rt();
        let ops: DelayedOps<String, i32> = DelayedOps::new(rt.handle().clone());
        let calls = Arc::new(AtomicUsize::new(0));

        // Slower than the first caller's budget: it defers.
        assert!(matches!(
            ops.resolve(
                "k".to_string(),
                Duration::from_millis(40),
                counted(&calls, Duration::from_millis(300), 42),
            ),
            DelayedResult::Pending
        ));
        // A second caller while the task still runs shares it: its closure is
        // never invoked (it gets 42, not 99) and the op runs once.
        assert!(matches!(
            ops.resolve(
                "k".to_string(),
                Duration::from_secs(2),
                counted(&calls, Duration::from_millis(300), 99),
            ),
            DelayedResult::Ready(42)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "single-flight: one run");
    }

    #[test]
    fn completed_op_leaves_no_slot_so_a_later_call_resolves_fresh() {
        let rt = multi_thread_rt();
        let ops: DelayedOps<String, i32> = DelayedOps::new(rt.handle().clone());
        let calls = Arc::new(AtomicUsize::new(0));

        assert!(matches!(
            ops.resolve(
                "k".to_string(),
                Duration::from_secs(2),
                counted(&calls, Duration::from_millis(0), 1),
            ),
            DelayedResult::Ready(1)
        ));
        // Let the slot removal settle, then resolve again: nothing is retained,
        // so the op re-runs and returns the fresh value (this is what lets
        // positive cache evidence beat an earlier negative result).
        std::thread::sleep(Duration::from_millis(50));
        assert!(matches!(
            ops.resolve(
                "k".to_string(),
                Duration::from_secs(2),
                counted(&calls, Duration::from_millis(0), 2),
            ),
            DelayedResult::Ready(2)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2, "no retention: re-resolved");
    }

    #[test]
    fn panicking_op_is_pending_then_recovers_on_retry() {
        let rt = multi_thread_rt();
        let ops: DelayedOps<String, i32> = DelayedOps::new(rt.handle().clone());

        // A task that panics before publishing must not wedge the key: the
        // caller sees Pending and the slot is cleared.
        assert!(matches!(
            ops.resolve("k".to_string(), Duration::from_secs(2), || async {
                panic!("boom")
            }),
            DelayedResult::Pending
        ));
        std::thread::sleep(Duration::from_millis(50));
        // The next caller re-resolves cleanly rather than attaching to a dead
        // slot and timing out forever.
        assert!(matches!(
            ops.resolve("k".to_string(), Duration::from_secs(2), || async { 7 }),
            DelayedResult::Ready(7)
        ));
    }
}
