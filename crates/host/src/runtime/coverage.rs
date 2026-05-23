use crate::path_prefix::path_prefix_matches;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

#[derive(Default)]
pub struct PathCoverageIndex<K> {
    by_key: HashMap<K, HashSet<String>>,
    by_path: HashMap<String, HashSet<K>>,
}

impl<K> PathCoverageIndex<K>
where
    K: Clone + Eq + Hash,
{
    pub fn insert(&mut self, key: K, path: String) {
        self.by_key
            .entry(key.clone())
            .or_default()
            .insert(path.clone());
        self.by_path.entry(path).or_default().insert(key);
    }

    pub fn extend<I>(&mut self, key: &K, paths: I)
    where
        I: IntoIterator<Item = String>,
    {
        for path in paths {
            self.insert(key.clone(), path);
        }
    }

    pub fn remove_key(&mut self, key: &K) -> Vec<String> {
        let Some(paths) = self.by_key.remove(key) else {
            return Vec::new();
        };

        for path in &paths {
            if let Some(keys) = self.by_path.get_mut(path) {
                keys.remove(key);
                if keys.is_empty() {
                    self.by_path.remove(path);
                }
            }
        }

        let mut paths = paths.into_iter().collect::<Vec<_>>();
        paths.sort();
        paths
    }

    pub fn remove_path(&mut self, path: &str) {
        let Some(keys) = self.by_path.remove(path) else {
            return;
        };
        for key in keys {
            if let Some(paths) = self.by_key.get_mut(&key) {
                paths.remove(path);
                if paths.is_empty() {
                    self.by_key.remove(&key);
                }
            }
        }
    }

    pub fn remove_prefix(&mut self, prefix: &str) {
        let paths = self
            .by_path
            .keys()
            .filter(|path| path_prefix_matches(prefix, path))
            .cloned()
            .collect::<Vec<_>>();
        for path in paths {
            self.remove_path(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PathCoverageIndex;
    #[test]
    fn coverage_index_removes_paths_by_prefix_with_segment_boundaries() {
        let mut index = PathCoverageIndex::default();
        let repo = ("repo".to_string(), "openai/gvfs".to_string());
        let sibling = ("repo".to_string(), "openai/gvfs-tools".to_string());

        index.insert(repo.clone(), "openai/gvfs".to_string());
        index.insert(repo.clone(), "openai/gvfs/issues".to_string());
        index.insert(sibling.clone(), "openai/gvfs-tools".to_string());

        index.remove_prefix("openai/gvfs");
        assert!(index.remove_key(&repo).is_empty());
        assert_eq!(
            index.remove_key(&sibling),
            vec!["openai/gvfs-tools".to_string()]
        );
    }
}
