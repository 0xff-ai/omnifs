use std::hash::Hash;

use dashmap::DashMap;

pub struct FollowSizeTable<Id = u64> {
    sizes: DashMap<Id, u64>,
}

impl<Id> Default for FollowSizeTable<Id>
where
    Id: Eq + Hash,
{
    fn default() -> Self {
        Self {
            sizes: DashMap::new(),
        }
    }
}

impl<Id> FollowSizeTable<Id>
where
    Id: Copy + Eq + Hash,
{
    pub fn grow(&self, id: Id, size: u64) {
        self.sizes
            .entry(id)
            .and_modify(|current| *current = (*current).max(size))
            .or_insert(size);
    }

    pub fn get(&self, id: Id) -> Option<u64> {
        self.sizes.get(&id).map(|entry| *entry.value())
    }

    pub fn remove(&self, id: Id) {
        self.sizes.remove(&id);
    }
}
