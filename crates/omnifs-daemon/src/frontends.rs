//! Live registry for frontends attached to the daemon's shared namespace.

use omnifs_api::{FrontendDelivery, FrontendInfo, FsType};
use omnifs_vfs_wire::{AttachObserver, FrontendIdentity, FrontendKind};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;

pub(crate) struct Frontends {
    stop_tx: mpsc::Sender<()>,
    stop_rx: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
    attached: Arc<AttachedRegistry>,
    on_change: Arc<dyn Fn(Vec<FrontendInfo>) + Send + Sync>,
}

#[derive(Debug, Clone)]
struct AttachedFrontend {
    kind: FrontendKind,
    mount_point: PathBuf,
    delivery: FrontendDelivery,
}

impl AttachedFrontend {
    fn key(&self) -> AttachmentKey {
        AttachmentKey {
            delivery: match self.delivery {
                FrontendDelivery::Local => 0,
                FrontendDelivery::Docker => 1,
                FrontendDelivery::Krunkit => 2,
            },
            kind: match self.kind {
                FrontendKind::Fuse => 0,
                FrontendKind::Nfs => 1,
            },
            mount_point: self.mount_point.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AttachmentKey {
    delivery: u8,
    kind: u8,
    mount_point: PathBuf,
}

struct AttachedEntry {
    frontend: AttachedFrontend,
    connections: usize,
}

struct AttachedState {
    next_id: u64,
    ids: BTreeMap<u64, AttachmentKey>,
    entries: BTreeMap<AttachmentKey, AttachedEntry>,
}

impl AttachedState {
    fn snapshot(&self) -> Vec<FrontendInfo> {
        self.entries
            .values()
            .map(|entry| FrontendInfo {
                source: "wire".to_string(),
                fs_type: match entry.frontend.kind {
                    FrontendKind::Fuse => FsType::Fuse,
                    FrontendKind::Nfs => FsType::Nfs,
                },
                mount_point: entry.frontend.mount_point.clone(),
                delivery: entry.frontend.delivery,
            })
            .collect()
    }
}

struct AttachedRegistry {
    state: std::sync::Mutex<AttachedState>,
}

impl AttachedRegistry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(AttachedState {
                next_id: 1,
                ids: BTreeMap::new(),
                entries: BTreeMap::new(),
            }),
        })
    }

    fn snapshot(&self) -> Vec<FrontendInfo> {
        self.state
            .lock()
            .expect("attached-frontend registry lock")
            .snapshot()
    }
}

struct DeliveryObserver {
    registry: Arc<AttachedRegistry>,
    delivery: FrontendDelivery,
    on_change: Arc<dyn Fn(Vec<FrontendInfo>) + Send + Sync>,
}

impl AttachObserver for DeliveryObserver {
    fn attached(&self, identity: &FrontendIdentity) -> u64 {
        let frontend = AttachedFrontend {
            kind: identity.kind,
            mount_point: identity.mount_point.clone(),
            delivery: self.delivery,
        };
        let key = frontend.key();
        let mut state = self
            .registry
            .state
            .lock()
            .expect("attached-frontend registry lock");
        let id = state.next_id;
        state.next_id += 1;
        state.ids.insert(id, key.clone());
        state
            .entries
            .entry(key)
            .and_modify(|entry| entry.connections += 1)
            .or_insert(AttachedEntry {
                frontend,
                connections: 1,
            });
        (self.on_change)(state.snapshot());
        id
    }

    fn detached(&self, id: u64) {
        let mut state = self
            .registry
            .state
            .lock()
            .expect("attached-frontend registry lock");
        let Some(key) = state.ids.remove(&id) else {
            return;
        };
        let remove = state.entries.get_mut(&key).is_some_and(|entry| {
            entry.connections -= 1;
            entry.connections == 0
        });
        if remove {
            state.entries.remove(&key);
        }
        (self.on_change)(state.snapshot());
    }
}

impl Frontends {
    pub(crate) fn new(on_change: Arc<dyn Fn(Vec<FrontendInfo>) + Send + Sync>) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel();
        Self {
            stop_tx,
            stop_rx: std::sync::Mutex::new(Some(stop_rx)),
            attached: AttachedRegistry::new(),
            on_change,
        }
    }

    pub(crate) fn attach_observer(&self, delivery: FrontendDelivery) -> Arc<dyn AttachObserver> {
        Arc::new(DeliveryObserver {
            registry: Arc::clone(&self.attached),
            delivery,
            on_change: Arc::clone(&self.on_change),
        })
    }

    pub(crate) fn attached(&self) -> Vec<FrontendInfo> {
        self.attached.snapshot()
    }

    pub(crate) fn serve(&self) {
        if let Some(rx) = self.stop_rx.lock().expect("stop rx lock").take() {
            let _ = rx.recv();
        }
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.stop_tx.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_reconnect_keeps_one_frontend_until_last_disconnect() {
        let frontends = Frontends::new(Arc::new(|_| {}));
        let observer = frontends.attach_observer(FrontendDelivery::Local);
        let identity = FrontendIdentity {
            kind: FrontendKind::Nfs,
            mount_point: PathBuf::from("/omnifs"),
        };

        let first = observer.attached(&identity);
        let second = observer.attached(&identity);
        assert_eq!(frontends.attached().len(), 1);

        observer.detached(first);
        assert_eq!(frontends.attached().len(), 1);

        observer.detached(second);
        assert!(frontends.attached().is_empty());
    }
}
