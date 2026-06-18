//! In-flight coalescing for provider namespace operations.
//!
//! When multiple FUSE threads race to resolve the same path, a naive
//! implementation issues a separate provider call per request. This module
//! deduplicates concurrent calls so the provider sees one request per path
//! until it resolves, and the leader's result is handed to waiters.
//!
//! Subtree semantics: if a strict ancestor of the requested path is
//! currently in flight, the request waits for that ancestor to complete
//! before proceeding. The ancestor may populate projection caches for
//! descendants, so the post-wait retry typically observes a cache hit.

use parking_lot::Mutex;
use std::collections::HashMap;
use tokio::sync::broadcast;

use omnifs_core::path::Path;
use omnifs_wit::provider::types as wit_types;

/// Shared outcome sent from a leader to waiters of the same path.
/// Errors are shared as their `Display` form since `Error`
/// wraps non-`Clone` sources; the unshared internal diagnostic is lost
/// by waiters but still present for the leader's own return path.
pub type SharedOutcome = std::result::Result<wit_types::OpResult, String>;

/// Tracks paths with an in-flight provider call so concurrent callers
/// coalesce instead of fanning out.
pub struct InFlight {
    map: Mutex<HashMap<Path, broadcast::Sender<SharedOutcome>>>,
}

/// What an `acquire` caller should do next.
pub enum Acquired<'a> {
    /// The caller is the unique leader for this path. The `guard`
    /// releases the slot on drop even if the leader aborts without
    /// calling `complete`, so waiters never hang on a dead sender.
    Leader { guard: LeaderGuard<'a> },
    /// Another caller is already resolving this exact path; wait for
    /// their result, which can be returned directly.
    ExactMatch {
        rx: broadcast::Receiver<SharedOutcome>,
    },
    /// A strict ancestor of the request is in flight. Wait for it to
    /// complete (the result is not ours to use), then retry acquire.
    AncestorWait {
        rx: broadcast::Receiver<SharedOutcome>,
    },
}

/// RAII slot handle for the leader. Must be consumed via `complete` on
/// success; dropping without completing releases the slot and lets any
/// waiters retry (they receive a channel-closed error from recv).
pub struct LeaderGuard<'a> {
    inflight: &'a InFlight,
    path: Path,
    armed: bool,
}

impl LeaderGuard<'_> {
    pub fn complete(mut self, outcome: SharedOutcome) {
        self.armed = false;
        let removed = {
            let mut map = self.inflight.map.lock();
            map.remove(&self.path)
        };
        if let Some(tx) = removed {
            let _ = tx.send(outcome);
        }
    }
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let mut map = self.inflight.map.lock();
            map.remove(&self.path);
        }
    }
}

impl InFlight {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Claim or join the in-flight slot for `path`.
    pub fn acquire(&self, path: &Path) -> Acquired<'_> {
        let mut map = self.map.lock();
        if let Some((key, tx)) = longest_ancestor(&map, path) {
            let rx = tx.subscribe();
            if key == path {
                return Acquired::ExactMatch { rx };
            }
            return Acquired::AncestorWait { rx };
        }
        let (tx, _) = broadcast::channel(1);
        map.insert(path.clone(), tx);
        Acquired::Leader {
            guard: LeaderGuard {
                inflight: self,
                path: path.clone(),
                armed: true,
            },
        }
    }
}

impl Default for InFlight {
    fn default() -> Self {
        Self::new()
    }
}

fn longest_ancestor<'a>(
    map: &'a HashMap<Path, broadcast::Sender<SharedOutcome>>,
    path: &Path,
) -> Option<(&'a Path, &'a broadcast::Sender<SharedOutcome>)> {
    let mut best: Option<(&Path, &broadcast::Sender<SharedOutcome>)> = None;
    for (k, tx) in map {
        if path.has_prefix(k)
            && best.is_none_or(|(existing, _)| k.as_str().len() > existing.as_str().len())
        {
            best = Some((k, tx));
        }
    }
    best
}

/// Wrap shareable outcomes so leaders and waiters see the same shape.
pub fn share_outcome<E: std::fmt::Display>(
    result: &std::result::Result<wit_types::OpResult, E>,
) -> SharedOutcome {
    match result {
        Ok(v) => Ok(v.clone()),
        Err(e) => Err(e.to_string()),
    }
}

/// Convert a waiter's shared outcome back into the caller's expected
/// `Result<OpResult, E>` shape using the supplied error constructor.
pub fn unshare_outcome<E>(
    outcome: SharedOutcome,
    make_err: impl FnOnce(String) -> E,
) -> std::result::Result<wit_types::OpResult, E> {
    match outcome {
        Ok(v) => Ok(v),
        Err(msg) => Err(make_err(msg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender() -> broadcast::Sender<SharedOutcome> {
        broadcast::channel(1).0
    }

    fn path(value: &str) -> Path {
        Path::parse(value).unwrap()
    }

    #[test]
    fn ancestor_match_prefers_longest() {
        let mut map = HashMap::new();
        map.insert(path("/a"), sender());
        map.insert(path("/a/b"), sender());
        let (k, _) = longest_ancestor(&map, &path("/a/b/c")).unwrap();
        assert_eq!(k.as_str(), "/a/b");
    }

    #[test]
    fn ancestor_match_requires_slash_boundary() {
        let mut map = HashMap::new();
        map.insert(path("/abc"), sender());
        assert!(longest_ancestor(&map, &path("/abcd")).is_none());
        assert!(longest_ancestor(&map, &path("/abc/d")).is_some());
        assert!(longest_ancestor(&map, &path("/abc")).is_some());
    }

    #[test]
    fn root_path_is_ancestor_of_descendants() {
        let mut map = HashMap::new();
        map.insert(Path::root(), sender());
        assert!(longest_ancestor(&map, &path("/a")).is_some());
        assert!(longest_ancestor(&map, &Path::root()).is_some());
    }

    #[test]
    fn acquire_returns_leader_when_slot_free() {
        let inflight = InFlight::new();
        let outcome = inflight.acquire(&path("/a/b"));
        assert!(matches!(outcome, Acquired::Leader { .. }));
    }

    #[test]
    fn acquire_returns_exact_match_when_same_path_in_flight() {
        let inflight = InFlight::new();
        let _leader = inflight.acquire(&path("/a/b"));
        let second = inflight.acquire(&path("/a/b"));
        assert!(matches!(second, Acquired::ExactMatch { .. }));
    }

    #[test]
    fn acquire_returns_ancestor_wait_when_parent_in_flight() {
        let inflight = InFlight::new();
        let _leader = inflight.acquire(&path("/a"));
        let descendant = inflight.acquire(&path("/a/b/c"));
        assert!(matches!(descendant, Acquired::AncestorWait { .. }));
    }

    #[test]
    fn acquire_treats_siblings_as_independent() {
        let inflight = InFlight::new();
        let _first = inflight.acquire(&path("/a/b"));
        let sibling = inflight.acquire(&path("/a/c"));
        assert!(matches!(sibling, Acquired::Leader { .. }));
    }

    #[tokio::test]
    async fn complete_delivers_outcome_to_waiters() {
        let inflight = InFlight::new();
        let Acquired::Leader { guard } = inflight.acquire(&path("/x")) else {
            panic!("first acquire should be leader");
        };
        let Acquired::ExactMatch { mut rx } = inflight.acquire(&path("/x")) else {
            panic!("second acquire should wait for exact match");
        };
        guard.complete(Err("oops".to_string()));
        let received = rx.recv().await.expect("waiter receives outcome");
        match received {
            Err(msg) => assert_eq!(msg, "oops"),
            Ok(_) => panic!("expected shared error outcome"),
        }
    }

    #[tokio::test]
    async fn dropping_leader_releases_slot_and_closes_waiters() {
        let inflight = InFlight::new();
        let Acquired::Leader { guard } = inflight.acquire(&path("/x")) else {
            panic!("first acquire should be leader");
        };
        let Acquired::ExactMatch { mut rx } = inflight.acquire(&path("/x")) else {
            panic!("second acquire should wait for exact match");
        };
        // Simulate leader aborting without completing.
        drop(guard);
        let err = rx.recv().await.expect_err("expected channel closed");
        assert!(matches!(err, broadcast::error::RecvError::Closed));
        // Slot is freed; new callers acquire as leader.
        assert!(matches!(
            inflight.acquire(&path("/x")),
            Acquired::Leader { .. }
        ));
    }
}
