//! Keyed materialization of sandbox output trees.

use crate::sandbox::publish;
use crate::tree_refs::TreeRefs;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Result of a materialization request.
#[derive(Debug)]
pub(crate) enum MaterializedTree<R> {
    /// The requested semantic view was already materialized.
    Cached { tree: u64 },
    /// The request ran the tool and published a new tree.
    Fresh { tree: u64, output: R },
}

/// Failure while materializing a tree.
#[derive(Debug)]
pub(crate) enum MaterializeError<E> {
    /// The cache root or temporary output directory could not be prepared.
    Prepare(std::io::Error),
    /// The tool itself rejected or failed the request.
    Run(E),
    /// The completed output directory could not be published.
    Publish(std::io::Error),
}

/// Semantic key for a materialized tool output tree.
pub(crate) trait TreeKey: Clone + Eq + Hash {
    /// Stable directory name for this semantic view under the materializer root.
    fn dir_name(&self) -> String;
}

/// Cache of tool-produced directory trees keyed by semantic view.
///
/// The cache coalesces concurrent materializations of the same key,
/// publishes completed trees through a sibling temp directory rename,
/// and registers the published path in the shared [`TreeRefs`].
pub(crate) struct TreeMaterializer<K> {
    root: PathBuf,
    trees: Arc<TreeRefs>,
    trees_by_key: DashMap<K, u64>,
    locks: DashMap<K, Arc<Mutex<()>>>,
}

impl<K> TreeMaterializer<K>
where
    K: TreeKey,
{
    /// Create a materializer rooted at `root` and sweep stale temp dirs.
    pub(crate) fn new(root: PathBuf, trees: Arc<TreeRefs>) -> Self {
        // Startup cleanup is best-effort; later writes report concrete
        // filesystem errors when the cache root is unusable.
        let _ = publish::sweep_temp_publish_dirs(&root);
        Self {
            root,
            trees,
            trees_by_key: DashMap::new(),
            locks: DashMap::new(),
        }
    }

    /// Return an existing tree for `key`, or run `materialize` into a
    /// temporary directory and publish the result.
    pub(crate) fn materialize<R, E>(
        &self,
        key: &K,
        materialize: impl FnOnce(&Path) -> Result<R, E>,
    ) -> Result<MaterializedTree<R>, MaterializeError<E>> {
        if let Some(id) = self.trees_by_key.get(key).map(|r| *r) {
            return Ok(MaterializedTree::Cached { tree: id });
        }

        let lock = self
            .locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock();

        if let Some(id) = self.trees_by_key.get(key).map(|r| *r) {
            self.locks.remove(key);
            return Ok(MaterializedTree::Cached { tree: id });
        }

        std::fs::create_dir_all(&self.root).map_err(MaterializeError::Prepare)?;
        let dest = self.root.join(key.dir_name());

        if let Ok(metadata) = std::fs::symlink_metadata(&dest) {
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                let tree = self.trees.register(dest);
                self.trees_by_key.insert(key.clone(), tree);
                self.locks.remove(key);
                return Ok(MaterializedTree::Cached { tree });
            }
            publish::remove_existing_path(&dest).map_err(MaterializeError::Prepare)?;
        }

        let tmp = publish::temp_sibling_path(&dest);
        if tmp.exists() {
            publish::remove_existing_path(&tmp).map_err(MaterializeError::Prepare)?;
        }
        std::fs::create_dir_all(&tmp).map_err(MaterializeError::Prepare)?;

        let output = match materialize(&tmp) {
            Ok(output) => output,
            Err(e) => {
                publish::remove_path_best_effort(&tmp);
                self.locks.remove(key);
                return Err(MaterializeError::Run(e));
            },
        };

        if let Err(e) = publish::publish_dir_by_rename(&tmp, &dest) {
            publish::remove_path_best_effort(&tmp);
            self.locks.remove(key);
            return Err(MaterializeError::Publish(e));
        }

        let tree = self.trees.register(dest);
        self.trees_by_key.insert(key.clone(), tree);
        self.locks.remove(key);
        Ok(MaterializedTree::Fresh { tree, output })
    }
}
