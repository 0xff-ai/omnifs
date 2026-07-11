//! Unified in-flight coalescing for host-side work.
//!
//! Callers supply a local [`CoverKey`] and a [`RunConfig`]. The engine dedupes
//! exact keys, optionally waits under covering keys, and exposes [`Acquired`]
//! states instead of scattering acquire/complete loops at every call site.
//!
//! The internal retry loop in [`Coalesce::run`] is not a hot loop: each
//! [`Acquired::Covered`] iteration awaits the covering slot's notifier before
//! re-trying, so progress is tied to in-flight work finishing, not CPU spinning.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, watch};

/// Exact slot identity in the engine map.
pub trait CoalesceKey: Clone + Send + Sync + 'static {
    type Id: Hash + Eq + Clone + Send;
    fn exact_id(&self) -> Self::Id;
}

/// Keys that may wait under a broader in-flight holder.
///
/// Exact-only keys use the default `cover` / `covers` / `prefer_cover` and
/// participate in exact dedupe only.
pub trait CoverKey: CoalesceKey {
    #[must_use]
    fn cover(&self) -> Self {
        self.clone()
    }

    fn covers(holder: &Self, waiter: &Self) -> bool {
        let _ = (holder, waiter);
        false
    }

    /// Among holders that cover `waiter`, return true when `candidate` is a better
    /// cover than `current`.
    fn prefer_cover(waiter: &Self, current: &Self, candidate: &Self) -> bool {
        let _ = (waiter, current, candidate);
        false
    }
}

/// How an exact-key follower waits for the running holder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Join until the holder publishes. `None` waits forever; `Some(d)` times out
    /// into [`Acquired::Pending`] while detached work continues.
    Block(Option<Duration>),
    /// Wait for a covering holder, then retry acquire. No result handoff.
    Wait,
}

/// Per-call coalescing behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunConfig {
    pub exact: Policy,
    pub cover: Policy,
}

impl RunConfig {
    pub const EXACT: Self = Self {
        exact: Policy::Block(None),
        cover: Policy::Block(None),
    };

    pub const NAMESPACE: Self = Self {
        exact: Policy::Block(None),
        cover: Policy::Wait,
    };

    pub fn budgeted(budget: Duration) -> Self {
        Self {
            exact: Policy::Block(Some(budget)),
            cover: Policy::Block(None),
        }
    }
}

/// What [`Coalesce::run`] or [`Coalesce::resolve`] observes before work runs.
pub enum Acquired<'a, K: CoverKey, V> {
    /// This caller should run the work and publish through `guard`.
    Run { guard: RunGuard<'a, K, V> },
    /// An exact-key follower; the shared result is ready.
    Ready { value: Arc<V> },
    /// An exact-key follower exceeded its block budget; work continues detached.
    Pending,
    /// A covering holder is in flight. Await `notifier`, then retry acquire.
    Covered { notifier: broadcast::Receiver<()> },
}

/// Outcome of a budgeted [`Coalesce::resolve`].
pub enum ResolveOutcome<V> {
    Ready(Arc<V>),
    Pending,
}

struct InFlightSlot<K: CoverKey, V> {
    key: K,
    result: watch::Sender<Option<Arc<V>>>,
    notifier: broadcast::Sender<()>,
}

struct SlotRef<K: CoverKey, V> {
    key_id: K::Id,
    result: watch::Receiver<Option<Arc<V>>>,
    notifier: broadcast::Sender<()>,
}

/// Tracks in-flight work keyed by caller-defined [`CoverKey`] types.
pub struct Coalesce<K: CoverKey, V> {
    slots: Mutex<HashMap<K::Id, InFlightSlot<K, V>>>,
    rt: Option<Handle>,
}

/// RAII handle for the caller currently running work. Must be consumed via
/// [`RunGuard::complete`]; dropping releases the slot without publishing.
pub struct RunGuard<'a, K: CoverKey, V> {
    coalesce: &'a Coalesce<K, V>,
    key_id: K::Id,
    armed: bool,
}

impl<K: CoverKey, V> Default for Coalesce<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: CoverKey, V> Coalesce<K, V> {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            rt: None,
        }
    }

    pub fn with_runtime(rt: Handle) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            rt: Some(rt),
        }
    }

    /// Run `work` once under `key`, coalescing exact matches and cover waits.
    ///
    /// Cover retries happen inside this method. Each retry awaits a covering
    /// holder's notifier, so this is not a busy spin at call sites.
    pub async fn run<W, Fut>(&self, key: &K, cfg: RunConfig, work: W) -> Arc<V>
    where
        W: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        loop {
            match self.acquire(key, cfg).await {
                Acquired::Run { guard } => {
                    let value = Arc::new(work().await);
                    guard.complete(value.clone());
                    return value;
                },
                Acquired::Ready { value } => return value,
                Acquired::Pending => {
                    panic!("Coalesce::run does not use a block budget; use resolve() instead")
                },
                Acquired::Covered { mut notifier } => {
                    let _ = notifier.recv().await;
                },
            }
        }
    }

    /// Budgeted exact-key coalescing with a detached runner.
    pub fn resolve<F, Fut>(
        self: &Arc<Self>,
        key: &K,
        budget: Duration,
        make: F,
    ) -> ResolveOutcome<V>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = V> + Send + 'static,
        V: Send + Sync + 'static,
    {
        let rt = self
            .rt
            .as_ref()
            .expect("Coalesce::resolve requires a runtime handle; use with_runtime()");
        let receiver = match self.claim(key) {
            Claim::Run {
                mut guard,
                receiver,
            } => {
                let key_id = guard.key_id.clone();
                guard.disarm();
                let coalesce = Arc::clone(self);
                rt.spawn(async move {
                    let value = Arc::new(make().await);
                    coalesce.publish(&key_id, value);
                });
                receiver
            },
            Claim::Join(slot) => slot.result,
            Claim::Covered(_) => {
                panic!("resolve() uses exact-key budgets only; cover waits are unsupported")
            },
        };
        match rt.block_on(async {
            tokio::time::timeout(budget, Self::wait_for_publish(receiver)).await
        }) {
            Ok(Some(value)) => ResolveOutcome::Ready(value),
            Ok(None) | Err(_) => ResolveOutcome::Pending,
        }
    }

    async fn acquire(&self, key: &K, cfg: RunConfig) -> Acquired<'_, K, V> {
        match self.claim(key) {
            Claim::Run { guard, receiver: _ } => Acquired::Run { guard },
            Claim::Join(slot) => self.wait_exact(slot, cfg.exact).await,
            Claim::Covered(notifier) => Acquired::Covered {
                notifier: notifier.subscribe(),
            },
        }
    }

    fn claim(&self, key: &K) -> Claim<'_, K, V> {
        let mut slots = self.slots.lock();
        let key_id = key.exact_id();

        if let Some(slot) = slots.get(&key_id)
            && slot.result.borrow().is_none()
        {
            return Claim::Join(SlotRef {
                key_id: key_id.clone(),
                result: slot.result.subscribe(),
                notifier: slot.notifier.clone(),
            });
        }

        if let Some(slot) = best_cover_slot(key, &slots) {
            return Claim::Covered(slot.notifier.clone());
        }

        let (result_tx, result_rx) = watch::channel(None);
        let (notify_tx, _) = broadcast::channel(1);
        slots.insert(
            key_id.clone(),
            InFlightSlot {
                key: key.clone(),
                result: result_tx,
                notifier: notify_tx,
            },
        );
        Claim::Run {
            guard: RunGuard {
                coalesce: self,
                key_id,
                armed: true,
            },
            receiver: result_rx,
        }
    }

    async fn wait_exact(&self, slot: SlotRef<K, V>, policy: Policy) -> Acquired<'_, K, V> {
        match policy {
            Policy::Block(None) => {
                if let Some(value) = Self::wait_for_publish(slot.result).await {
                    Acquired::Ready { value }
                } else {
                    self.release_slot(&slot.key_id);
                    Acquired::Run {
                        guard: RunGuard {
                            coalesce: self,
                            key_id: slot.key_id,
                            armed: true,
                        },
                    }
                }
            },
            Policy::Block(Some(budget)) => {
                match tokio::time::timeout(budget, Self::wait_for_publish(slot.result)).await {
                    Ok(Some(value)) => Acquired::Ready { value },
                    Ok(None) | Err(_) => Acquired::Pending,
                }
            },
            Policy::Wait => Acquired::Covered {
                notifier: slot.notifier.subscribe(),
            },
        }
    }

    async fn wait_for_publish(mut result: watch::Receiver<Option<Arc<V>>>) -> Option<Arc<V>> {
        loop {
            if let Some(value) = result.borrow_and_update().clone() {
                return Some(value);
            }
            if result.changed().await.is_err() {
                return result.borrow().clone();
            }
        }
    }

    fn release_slot(&self, key_id: &K::Id) {
        self.slots.lock().remove(key_id);
    }

    fn publish(&self, key_id: &K::Id, value: Arc<V>) {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get_mut(key_id) {
            let _ = slot.result.send(Some(value));
            let _ = slot.notifier.send(());
        }
        slots.remove(key_id);
    }
}

enum Claim<'a, K: CoverKey, V> {
    Run {
        guard: RunGuard<'a, K, V>,
        receiver: watch::Receiver<Option<Arc<V>>>,
    },
    Join(SlotRef<K, V>),
    Covered(broadcast::Sender<()>),
}

fn best_cover_slot<'a, K, V>(
    waiter: &K,
    slots: &'a HashMap<K::Id, InFlightSlot<K, V>>,
) -> Option<&'a InFlightSlot<K, V>>
where
    K: CoverKey,
{
    let mut best: Option<&InFlightSlot<K, V>> = None;
    for slot in slots.values() {
        if slot.result.borrow().is_some() {
            continue;
        }
        if !K::covers(&slot.key, waiter) {
            continue;
        }
        match best {
            None => best = Some(slot),
            Some(current) if K::prefer_cover(waiter, &current.key, &slot.key) => best = Some(slot),
            _ => {},
        }
    }
    best
}

impl<K, V> RunGuard<'_, K, V>
where
    K: CoverKey,
{
    pub fn complete(mut self, value: Arc<V>) {
        self.disarm();
        self.coalesce.publish(&self.key_id, value);
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<K: CoverKey, V> Drop for RunGuard<'_, K, V> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let notifier = self
            .coalesce
            .slots
            .lock()
            .get(&self.key_id)
            .map(|slot| slot.notifier.clone());
        self.coalesce.release_slot(&self.key_id);
        if let Some(notifier) = notifier {
            let _ = notifier.send(());
        }
    }
}

/// Namespace provider-op keys. Lives here so [`crate::runtime::Runtime`] can own
/// the shared [`Coalesce`] without a namespace ↔ runtime cycle.
#[cfg(feature = "runtime")]
pub mod ns {
    use super::{CoalesceKey, CoverKey};
    use crate::object_id::ObjectId;
    use omnifs_core::path::Path;
    use omnifs_wit::provider::types as wit_types;

    /// Shared outcome sent from the running caller to exact-key waiters.
    pub type SharedOutcome = std::result::Result<wit_types::OpResult, String>;

    /// Key a namespace provider op coalesces under.
    #[derive(Clone, PartialEq, Eq, Hash)]
    pub enum Key {
        Path(Path),
        Object(ObjectId),
        Revalidate(ObjectId),
    }

    impl CoalesceKey for Key {
        type Id = Key;

        fn exact_id(&self) -> Key {
            self.clone()
        }
    }

    impl CoverKey for Key {
        fn covers(holder: &Self, waiter: &Self) -> bool {
            match (holder, waiter) {
                (Self::Path(holder), Self::Path(waiter)) => {
                    waiter.has_prefix(holder) && holder != waiter
                },
                _ => false,
            }
        }

        fn prefer_cover(_waiter: &Self, current: &Self, candidate: &Self) -> bool {
            match (current, candidate) {
                (Self::Path(current), Self::Path(candidate)) => {
                    candidate.segments().count() > current.segments().count()
                },
                _ => false,
            }
        }
    }

    pub fn share_outcome<E: std::fmt::Display>(
        result: std::result::Result<wit_types::OpResult, E>,
    ) -> SharedOutcome {
        result.map_err(|error| error.to_string())
    }

    pub fn unshare_outcome<E>(
        outcome: SharedOutcome,
        make_err: impl FnOnce(String) -> E,
    ) -> std::result::Result<wit_types::OpResult, E> {
        match outcome {
            Ok(v) => Ok(v),
            Err(msg) => Err(make_err(msg)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_core::path::Path;

    #[derive(Clone, Eq, Hash, PartialEq)]
    struct PathKey(Path);

    impl CoalesceKey for PathKey {
        type Id = Path;
        fn exact_id(&self) -> Path {
            self.0.clone()
        }
    }

    impl CoverKey for PathKey {
        fn covers(holder: &Self, waiter: &Self) -> bool {
            waiter.0.has_prefix(&holder.0) && holder.0 != waiter.0
        }

        fn prefer_cover(_waiter: &Self, current: &Self, candidate: &Self) -> bool {
            candidate.0.segments().count() > current.0.segments().count()
        }
    }

    fn path(value: &str) -> PathKey {
        PathKey(Path::parse(value).unwrap())
    }

    #[test]
    fn cover_match_prefers_longest() {
        let mut slots: HashMap<Path, InFlightSlot<PathKey, ()>> = HashMap::new();
        let (tx, _): (watch::Sender<Option<Arc<()>>>, _) = watch::channel(None);
        let (notify, _) = broadcast::channel(1);
        slots.insert(
            path("/a").exact_id(),
            InFlightSlot {
                key: path("/a"),
                result: tx.clone(),
                notifier: notify.clone(),
            },
        );
        let (tx2, _) = watch::channel(None);
        slots.insert(
            path("/a/b").exact_id(),
            InFlightSlot {
                key: path("/a/b"),
                result: tx2,
                notifier: notify,
            },
        );
        let best = best_cover_slot(&path("/a/b/c"), &slots).unwrap();
        assert_eq!(best.key.0.as_str(), "/a/b");
    }

    #[test]
    fn cover_match_requires_slash_boundary() {
        let mut slots: HashMap<Path, InFlightSlot<PathKey, ()>> = HashMap::new();
        let (tx, _): (watch::Sender<Option<Arc<()>>>, _) = watch::channel(None);
        let (notify, _) = broadcast::channel(1);
        slots.insert(
            path("/abc").exact_id(),
            InFlightSlot {
                key: path("/abc"),
                result: tx,
                notifier: notify,
            },
        );
        assert!(best_cover_slot(&path("/abcd"), &slots).is_none());
        assert!(best_cover_slot(&path("/abc/d"), &slots).is_some());
    }

    #[tokio::test]
    async fn run_coalesces_exact_matches() {
        let coalesce = Arc::new(Coalesce::<PathKey, u32>::new());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let leader = {
            let coalesce = Arc::clone(&coalesce);
            tokio::spawn(async move {
                coalesce
                    .run(&path("/x"), RunConfig::EXACT, || async {
                        c1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        7
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        let c2 = Arc::clone(&calls);
        let follower = coalesce
            .run(&path("/x"), RunConfig::EXACT, || async {
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                9
            })
            .await;
        assert_eq!(*leader.await.unwrap(), 7);
        assert_eq!(*follower, 7);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn run_waits_under_cover_then_retries() {
        let coalesce = Arc::new(Coalesce::<PathKey, u32>::new());
        let parent = {
            let coalesce = Arc::clone(&coalesce);
            tokio::spawn(async move {
                coalesce
                    .run(&path("/a"), RunConfig::NAMESPACE, || async {
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        1
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        let child = coalesce
            .run(&path("/a/b"), RunConfig::NAMESPACE, || async { 2 })
            .await;
        assert_eq!(*parent.await.unwrap(), 1);
        assert_eq!(*child, 2);
    }

    #[derive(Clone, Eq, Hash, PartialEq)]
    struct StrKey(&'static str);

    impl CoalesceKey for StrKey {
        type Id = &'static str;
        fn exact_id(&self) -> &'static str {
            self.0
        }
    }

    impl CoverKey for StrKey {}

    fn multi_thread_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("multi-thread runtime")
    }

    #[test]
    fn concurrent_resolve_does_not_panic_when_runner_finishes_fast() {
        let rt = multi_thread_rt();
        let coalesce = Arc::new(Coalesce::with_runtime(rt.handle().clone()));
        let key = StrKey("dir");
        let ready = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        match coalesce.resolve(&key, Duration::from_millis(200), {
                            let ready = Arc::clone(&ready);
                            move || async move {
                                ready.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                1u32
                            }
                        }) {
                            ResolveOutcome::Ready(value) => assert_eq!(*value, 1),
                            ResolveOutcome::Pending => {},
                        }
                    }
                });
            }
        });
        assert!(ready.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    }

    #[test]
    fn resolve_returns_pending_and_shares_one_runner() {
        let rt = multi_thread_rt();
        let coalesce = Arc::new(Coalesce::with_runtime(rt.handle().clone()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let key = StrKey("k");

        assert!(matches!(
            coalesce.resolve(&key, Duration::from_millis(40), {
                let calls = Arc::clone(&calls);
                move || async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    1u32
                }
            }),
            ResolveOutcome::Pending
        ));
        match coalesce.resolve(&key, Duration::from_secs(2), {
            let calls = Arc::clone(&calls);
            move || async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                2u32
            }
        }) {
            ResolveOutcome::Ready(value) => assert_eq!(*value, 1),
            ResolveOutcome::Pending => panic!("expected the shared runner to finish"),
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn resolve_purges_slot_so_later_call_reruns() {
        let rt = multi_thread_rt();
        let coalesce = Arc::new(Coalesce::with_runtime(rt.handle().clone()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let key = StrKey("k");

        match coalesce.resolve(&key, Duration::from_secs(2), {
            let calls = Arc::clone(&calls);
            move || async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                1u32
            }
        }) {
            ResolveOutcome::Ready(value) => assert_eq!(*value, 1),
            ResolveOutcome::Pending => panic!("expected ready"),
        }
        std::thread::sleep(Duration::from_millis(50));
        match coalesce.resolve(&key, Duration::from_secs(2), {
            let calls = Arc::clone(&calls);
            move || async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                2u32
            }
        }) {
            ResolveOutcome::Ready(value) => assert_eq!(*value, 2),
            ResolveOutcome::Pending => panic!("expected ready"),
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn object_keys_coalesce_exact_match_only() {
        use crate::object_id::ObjectId;

        let coalesce = Arc::new(Coalesce::<ns::Key, u32>::new());
        let a = ns::Key::Object(ObjectId::from_bytes(b"issue:42".to_vec()));
        let b = ns::Key::Object(ObjectId::from_bytes(b"issue:4".to_vec()));
        let hold = {
            let coalesce = Arc::clone(&coalesce);
            let a = a.clone();
            tokio::spawn(async move {
                coalesce
                    .run(&a, RunConfig::EXACT, || async {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        1
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(*coalesce.run(&a, RunConfig::EXACT, || async { 2 }).await, 1);
        assert_eq!(*coalesce.run(&b, RunConfig::EXACT, || async { 3 }).await, 3);
        hold.await.unwrap();
    }

    #[tokio::test]
    async fn aborting_runner_releases_slot() {
        use tokio::sync::Notify;

        let coalesce = Arc::new(Coalesce::<PathKey, u32>::new());
        let started = Arc::new(Notify::new());
        let blocker = Arc::new(Notify::new());
        let leader = {
            let coalesce = Arc::clone(&coalesce);
            let started = Arc::clone(&started);
            let blocker = Arc::clone(&blocker);
            tokio::spawn(async move {
                coalesce
                    .run(&path("/x"), RunConfig::EXACT, || async {
                        started.notify_one();
                        blocker.notified().await;
                        1
                    })
                    .await
            })
        };
        started.notified().await;
        leader.abort();
        let _ = leader.await;
        let recovered = tokio::time::timeout(
            Duration::from_millis(200),
            coalesce.run(&path("/x"), RunConfig::EXACT, || async { 2 }),
        )
        .await
        .expect("slot should be free after runner abort");
        assert_eq!(*recovered, 2);
    }
}
