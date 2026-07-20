//! Bounded NFS protocol reply cache.
//!
//! The engine owns every reuse decision through namespace TTLs. This
//! module only retains plain namespace answers for the NFS client, whose mount
//! options deliberately disable the kernel attribute and negative-name caches.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::Instant;

use omnifs_core::path::Path;
use omnifs_engine::namespace::{Attrs, DirEntry, LookupAnswer};

const MAX_ATTRS: usize = 32_768;
const MAX_LOOKUPS: usize = 32_768;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NameKey {
    parent: Path,
    name: String,
}

struct Cached<T> {
    value: T,
    expires_at: Instant,
}

impl<T> Cached<T> {
    fn new(value: T, ttl: std::time::Duration) -> Option<Self> {
        if ttl.is_zero() {
            return None;
        }
        let expires_at = Instant::now().checked_add(ttl)?;
        Some(Self { value, expires_at })
    }

    fn is_fresh(&self, now: Instant) -> bool {
        now < self.expires_at
    }
}

// ponytail: arbitrary eviction is enough for a hard memory bound; use an LRU
// only if measured churn evicts hot entries.
struct BoundedMap<K, V> {
    values: HashMap<K, V>,
    limit: usize,
}

impl<K, V> BoundedMap<K, V>
where
    K: Clone + Eq + Hash,
{
    fn new(limit: usize) -> Self {
        Self {
            values: HashMap::with_capacity(limit),
            limit,
        }
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.values.get(key)
    }

    fn insert(&mut self, key: K, value: V) {
        if self.values.len() >= self.limit
            && !self.values.contains_key(&key)
            && let Some(evicted) = self.values.keys().next().cloned()
        {
            self.values.remove(&evicted);
        }
        self.values.insert(key, value);
    }

    fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        self.values.retain(|key, value| keep(key, value));
    }

    fn clear(&mut self) {
        self.values.clear();
    }
}

struct State {
    generation: u64,
    attrs: BoundedMap<Path, Cached<Attrs>>,
    lookups: BoundedMap<NameKey, Cached<LookupAnswer>>,
}

pub(crate) struct ReplyCache {
    state: Mutex<State>,
}

impl ReplyCache {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                generation: 0,
                attrs: BoundedMap::new(MAX_ATTRS),
                lookups: BoundedMap::new(MAX_LOOKUPS),
            }),
        }
    }

    pub(crate) fn fence(&self) -> u64 {
        self.state.lock().expect("NFS reply cache").generation
    }

    pub(crate) fn attrs(&self, path: &Path) -> Option<Attrs> {
        let state = self.state.lock().expect("NFS reply cache");
        state
            .attrs
            .get(path)
            .filter(|cached| cached.is_fresh(Instant::now()))
            .map(|cached| cached.value.clone())
    }

    pub(crate) fn lookup(&self, parent: &Path, name: &str) -> Option<LookupAnswer> {
        let key = NameKey {
            parent: parent.clone(),
            name: name.to_string(),
        };
        let state = self.state.lock().expect("NFS reply cache");
        state
            .lookups
            .get(&key)
            .filter(|cached| cached.is_fresh(Instant::now()))
            .map(|cached| cached.value.clone())
    }

    pub(crate) fn remember_attrs(&self, fence: u64, path: Path, attrs: &Attrs) {
        let Some(cached) = Cached::new(attrs.clone(), attrs.ttl) else {
            return;
        };
        let mut state = self.state.lock().expect("NFS reply cache");
        if state.accepts(fence) {
            state.attrs.insert(path, cached);
        }
    }

    pub(crate) fn remember_lookup(
        &self,
        fence: u64,
        parent: Path,
        name: String,
        answer: &LookupAnswer,
    ) {
        let attrs = answer.attrs().and_then(|attrs| {
            Cached::new(attrs.clone(), attrs.ttl).map(|cached| (answer.path.clone(), cached))
        });
        let Some(lookup) = Cached::new(answer.clone(), answer.ttl()) else {
            return;
        };

        let mut state = self.state.lock().expect("NFS reply cache");
        if !state.accepts(fence) {
            return;
        }
        if let Some((path, cached)) = attrs {
            state.attrs.insert(path, cached);
        }
        state.lookups.insert(NameKey { parent, name }, lookup);
    }

    pub(crate) fn seed(&self, fence: u64, parent: &Path, entries: &[DirEntry]) {
        for entry in entries {
            self.remember_attrs(fence, entry.path.clone(), &entry.attrs);
            self.remember_lookup(
                fence,
                parent.clone(),
                entry.name.clone(),
                &LookupAnswer::found(entry.path.clone(), entry.attrs.clone()),
            );
        }
    }

    pub(crate) fn invalidate(&self, path: &Path) {
        let mut state = self.state.lock().expect("NFS reply cache");
        state.generation = state.generation.wrapping_add(1);

        if path.is_root() {
            state.attrs.clear();
            state.lookups.clear();
            return;
        }
        state
            .attrs
            .retain(|cached_path, _| !cached_path.has_prefix(path));
        state
            .lookups
            .retain(|_, cached| !cached.value.path.has_prefix(path));
    }
}

impl State {
    fn accepts(&self, fence: u64) -> bool {
        fence == self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_engine::namespace::{EntryKind, ReadStyle, StabilityClass};
    use std::time::Duration;

    fn path(value: &str) -> Path {
        Path::parse(value).expect("test path")
    }

    fn attrs(ttl: Duration) -> Attrs {
        Attrs {
            kind: EntryKind::File,
            dev: 0,
            ino: 0,
            size: 1,
            blocks: 1,
            mode: 0o444,
            nlink: 1,
            accessed: None,
            modified: None,
            created: None,
            ttl,
            change: 0,
            direct_io: false,
            stability: StabilityClass::Stable,
            read_style: ReadStyle::Whole,
        }
    }

    #[test]
    fn zero_ttl_is_not_retained() {
        let cache = ReplyCache::new();
        let node = path("/mount/file");
        cache.remember_attrs(cache.fence(), node.clone(), &attrs(Duration::ZERO));
        assert!(cache.attrs(&node).is_none());
    }

    #[test]
    fn observed_invalidation_rejects_late_fill() {
        let cache = ReplyCache::new();
        let node = path("/mount/file");
        let fence = cache.fence();
        cache.invalidate(&node);
        cache.remember_attrs(fence, node.clone(), &attrs(Duration::from_mins(1)));
        assert!(cache.attrs(&node).is_none());
    }

    #[test]
    fn subtree_invalidation_removes_positive_and_negative_names() {
        let cache = ReplyCache::new();
        let parent = path("/mount/dir");
        let found_path = path("/mount/dir/found");
        let missing_path = path("/mount/dir/missing");
        let fence = cache.fence();
        cache.remember_lookup(
            fence,
            parent.clone(),
            "found".to_string(),
            &LookupAnswer::found(found_path, attrs(Duration::from_mins(1))),
        );
        cache.remember_lookup(
            fence,
            parent.clone(),
            "missing".to_string(),
            &LookupAnswer::missing(missing_path, Duration::from_mins(1)),
        );

        assert!(matches!(
            cache.lookup(&parent, "missing"),
            Some(LookupAnswer {
                state: omnifs_engine::namespace::LookupState::Missing { .. },
                ..
            })
        ));
        cache.invalidate(&parent);
        assert!(cache.lookup(&parent, "found").is_none());
        assert!(cache.lookup(&parent, "missing").is_none());
    }

    #[test]
    fn bounded_map_never_exceeds_limit() {
        let mut map = BoundedMap::new(2);
        map.insert(1, "one");
        map.insert(2, "two");
        map.insert(3, "three");

        assert_eq!(map.values.len(), 2);
        assert_eq!(map.values.get(&3), Some(&"three"));
    }
}
