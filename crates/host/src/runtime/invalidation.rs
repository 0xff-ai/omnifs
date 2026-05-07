use super::{CalloutRuntime, NotifierHandle};
use crate::path_key::{PathKey, PathToInode};
#[cfg(target_os = "linux")]
use crate::path_prefix::path_prefix_matches;
#[cfg(target_os = "linux")]
use fuser::INodeNo;
use parking_lot::Mutex;
#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::sync::Arc;

#[derive(Clone)]
struct InvalidationHandles {
    path_to_inode: Arc<PathToInode>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    notifier: NotifierHandle,
    mount: String,
}

#[derive(Default)]
pub(super) struct InvalidationState {
    invalidated_prefixes: Mutex<Vec<String>>,
    invalidated_paths: Mutex<Vec<String>>,
    handles: Mutex<Option<InvalidationHandles>>,
}

impl InvalidationState {
    fn install(&self, path_to_inode: Arc<PathToInode>, notifier: NotifierHandle, mount: String) {
        *self.handles.lock() = Some(InvalidationHandles {
            path_to_inode,
            notifier,
            mount,
        });
    }

    fn handles(&self) -> Option<InvalidationHandles> {
        self.handles.lock().clone()
    }

    pub(super) fn record_prefix(&self, prefix: String) {
        self.invalidated_prefixes.lock().push(prefix);
    }

    pub(super) fn record_path(&self, path: String) {
        self.invalidated_paths.lock().push(path);
    }

    fn drain_prefixes(&self) -> Vec<String> {
        let mut prefixes = self.invalidated_prefixes.lock();
        std::mem::take(&mut *prefixes)
    }

    fn drain_paths(&self) -> Vec<String> {
        let mut paths = self.invalidated_paths.lock();
        std::mem::take(&mut *paths)
    }
}

impl CalloutRuntime {
    pub fn install_invalidation(
        &self,
        path_to_inode: Arc<PathToInode>,
        notifier: NotifierHandle,
        mount: String,
    ) {
        self.invalidation.install(path_to_inode, notifier, mount);
    }

    // FUSE owns the in-memory L0 browse cache; the runtime only clears
    // shared indexes, L2 records, and kernel-facing path state.
    pub fn cache_delete_prefix(&self, prefix: &str) {
        self.activity_table
            .lock()
            .remove_prefix(&super::absolute_mount_path(prefix));

        if let Some(ref l2) = self.l2
            && let Err(e) = l2.delete_prefix(prefix)
        {
            tracing::debug!(prefix, error = %e, "L2 cache prefix delete failed");
        }

        #[cfg(target_os = "linux")]
        {
            let Some(handles) = self.invalidation.handles() else {
                return;
            };

            for entry in handles.path_to_inode.iter() {
                let (key, _) = entry.pair();
                if key.mount != handles.mount || !path_prefix_matches(prefix, &key.path) {
                    continue;
                }
                let Some((parent_path, child_name)) = key.path.rsplit_once('/') else {
                    continue;
                };
                let parent_ino = handles
                    .path_to_inode
                    .get(&PathKey::new(handles.mount.clone(), parent_path))
                    .map_or(1, |r| *r.value());
                if let Some(notifier) = handles.notifier.lock().as_ref() {
                    let _ = notifier.inval_entry(INodeNo(parent_ino), OsStr::new(child_name));
                }
            }
        }
    }

    pub fn cache_delete_path(&self, path: &str) {
        self.activity_table
            .lock()
            .remove_path(&super::absolute_mount_path(path));

        if let Some(handles) = self.invalidation.handles() {
            let _ = handles
                .path_to_inode
                .remove(&PathKey::new(handles.mount.clone(), path.to_string()));
        }

        if let Some(ref l2) = self.l2
            && let Err(e) = l2.delete_exact(path)
        {
            tracing::debug!(path, error = %e, "L2 cache exact delete failed");
        }

        #[cfg(target_os = "linux")]
        {
            let Some(handles) = self.invalidation.handles() else {
                return;
            };
            let Some((parent_path, child_name)) = path.rsplit_once('/') else {
                return;
            };
            let parent_ino = handles
                .path_to_inode
                .get(&PathKey::new(
                    handles.mount.clone(),
                    parent_path.to_string(),
                ))
                .map_or(1, |r| *r.value());
            if let Some(notifier) = handles.notifier.lock().as_ref() {
                let _ = notifier.inval_entry(INodeNo(parent_ino), OsStr::new(child_name));
            }
        }
    }

    pub fn drain_invalidated_prefixes(&self) -> Vec<String> {
        self.invalidation.drain_prefixes()
    }

    pub fn drain_invalidated_paths(&self) -> Vec<String> {
        self.invalidation.drain_paths()
    }
}
