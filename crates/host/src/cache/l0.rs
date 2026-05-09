//! L0 browse cache: in-memory, path-keyed, byte-weighted moka cache.

use crate::cache::{CacheRecord, Key, L0_MAX_WEIGHT, L0_SKIP_THRESHOLD};
use moka::sync::Cache as MokaCache;
use std::sync::Arc;

pub struct Cache {
    cache: MokaCache<Key, Arc<CacheRecord>>,
}

impl Cache {
    pub fn new() -> Self {
        let cache = MokaCache::builder()
            .max_capacity(L0_MAX_WEIGHT)
            .support_invalidation_closures()
            .weigher(|key: &Key, value: &Arc<CacheRecord>| -> u32 {
                let key_size = 1 + key.path.len() + key.aux.as_ref().map_or(0, String::len);
                let val_size = 2 + value.payload.len();
                (key_size + val_size).try_into().unwrap_or(u32::MAX)
            })
            .build();
        Self { cache }
    }

    pub fn get(&self, key: &Key) -> Option<Arc<CacheRecord>> {
        self.cache.get(key)
    }

    pub fn put(&self, key: Key, record: CacheRecord) {
        if record.payload.len() > L0_SKIP_THRESHOLD {
            return;
        }
        self.cache.insert(key, Arc::new(record));
    }

    pub fn invalidate(&self, key: &Key) {
        self.cache.invalidate(key);
    }

    pub fn invalidate_entries_if<P>(&self, predicate: P)
    where
        P: Fn(&Key, &Arc<CacheRecord>) -> bool + Send + Sync + 'static,
    {
        self.cache
            .invalidate_entries_if(predicate)
            .expect("invalidation closures enabled at cache construction");
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}
