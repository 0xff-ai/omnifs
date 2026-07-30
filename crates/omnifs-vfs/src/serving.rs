//! Epoch-stamped serving leases shared by the engine and wire server.

use crate::{Namespace, NsError, NsEvent};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, watch};

/// Identity and ordered publication sequence of one served namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NamespaceEpoch {
    daemon_instance: [u8; 16],
    sequence: NonZeroU64,
}

impl NamespaceEpoch {
    #[must_use]
    pub const fn new(daemon_instance: [u8; 16], sequence: NonZeroU64) -> Self {
        Self {
            daemon_instance,
            sequence,
        }
    }

    #[must_use]
    pub const fn initial(daemon_instance: [u8; 16]) -> Self {
        Self::new(daemon_instance, NonZeroU64::MIN)
    }

    pub fn next(self) -> Result<Self, NamespaceEpochOverflow> {
        let sequence = self
            .sequence
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(NamespaceEpochOverflow)?;
        Ok(Self::new(self.daemon_instance, sequence))
    }

    #[must_use]
    pub const fn daemon_instance(self) -> [u8; 16] {
        self.daemon_instance
    }

    #[must_use]
    pub fn relation_to(self, other: Self) -> EpochRelation {
        if self.daemon_instance == other.daemon_instance {
            match self.sequence.cmp(&other.sequence) {
                std::cmp::Ordering::Less => EpochRelation::Older,
                std::cmp::Ordering::Equal => EpochRelation::Same,
                std::cmp::Ordering::Greater => EpochRelation::Newer,
            }
        } else {
            EpochRelation::DifferentInstance
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochRelation {
    Older,
    Same,
    Newer,
    DifferentInstance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("namespace epoch sequence exhausted")]
pub struct NamespaceEpochOverflow;

/// One namespace request lease. Its private fields keep the namespace and
/// admission guard inseparable until the request finishes.
pub struct NamespaceLease {
    epoch: NamespaceEpoch,
    namespace: Arc<dyn Namespace>,
    _guard: Box<dyn Send + Sync>,
    cancellation: watch::Receiver<bool>,
}

impl NamespaceLease {
    pub fn new<G>(
        epoch: NamespaceEpoch,
        namespace: Arc<dyn Namespace>,
        guard: G,
        cancellation: watch::Receiver<bool>,
    ) -> Self
    where
        G: Send + Sync + 'static,
    {
        Self {
            epoch,
            namespace,
            _guard: Box::new(guard),
            cancellation,
        }
    }

    #[must_use]
    pub const fn epoch(&self) -> NamespaceEpoch {
        self.epoch
    }

    #[must_use]
    pub fn namespace(&self) -> &dyn Namespace {
        self.namespace.as_ref()
    }

    pub async fn run<F, T>(&self, future: F) -> Result<T, NsError>
    where
        F: std::future::Future<Output = Result<T, NsError>>,
    {
        tokio::pin!(future);
        let mut cancellation = self.cancellation.clone();
        loop {
            if *cancellation.borrow() {
                return Err(NsError::Network);
            }
            tokio::select! {
                result = &mut future => return result,
                changed = cancellation.changed() => {
                    if changed.is_err() {
                        return future.await;
                    }
                }
            }
        }
    }
}

/// Namespace source whose requests are guarded by one active generation.
pub trait ServingNamespace: Send + Sync {
    fn acquire(&self) -> Result<NamespaceLease, NsError>;
    fn subscribe(&self) -> NamespaceSubscription;
    fn current_epoch(&self) -> NamespaceEpoch;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceEvent {
    epoch: NamespaceEpoch,
    event: NsEvent,
}

impl NamespaceEvent {
    #[must_use]
    pub const fn epoch(&self) -> NamespaceEpoch {
        self.epoch
    }

    #[must_use]
    pub fn into_event(self) -> NsEvent {
        self.event
    }

    pub fn reset(epoch: NamespaceEpoch) -> Self {
        Self {
            epoch,
            event: NsEvent::reset(),
        }
    }
}

/// Shared atomic publication and event stream for a serving namespace.
pub struct NamespaceEventHub {
    state: Mutex<EventHubState>,
}

struct EventHubState {
    current: NamespaceEpoch,
    sender: broadcast::Sender<NamespaceEvent>,
}

impl NamespaceEventHub {
    #[must_use]
    pub fn new(initial: NamespaceEpoch, capacity: usize) -> Arc<Self> {
        let (sender, _) = broadcast::channel(capacity);
        Arc::new(Self {
            state: Mutex::new(EventHubState {
                current: initial,
                sender,
            }),
        })
    }

    #[must_use]
    pub fn current_epoch(&self) -> NamespaceEpoch {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
    }

    /// Capture the current epoch and subscribe while holding the same lock that
    /// publication uses, so a subscriber cannot miss the reset for a swap.
    #[must_use]
    pub fn subscribe(self: &Arc<Self>) -> NamespaceSubscription {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        NamespaceSubscription {
            initial_epoch: state.current,
            receiver: state.sender.subscribe(),
            hub: Arc::clone(self),
        }
    }

    /// Publish a new current epoch and its root reset as one locked action.
    pub fn advance(&self, next: NamespaceEpoch) -> Result<(), AdvanceEpochError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match next.relation_to(state.current) {
            EpochRelation::Newer | EpochRelation::DifferentInstance => {},
            EpochRelation::Older | EpochRelation::Same => {
                return Err(AdvanceEpochError {
                    current: state.current,
                    next,
                });
            },
        }
        state.current = next;
        let _ = state.sender.send(NamespaceEvent::reset(next));
        Ok(())
    }

    /// Forward an engine event only while its generation is still current.
    pub fn publish_if_current(&self, epoch: NamespaceEpoch, event: NsEvent) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.current != epoch {
            return false;
        }
        let _ = state.sender.send(NamespaceEvent { epoch, event });
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cannot publish namespace epoch {next:?} over {current:?}")]
pub struct AdvanceEpochError {
    pub current: NamespaceEpoch,
    pub next: NamespaceEpoch,
}

pub struct NamespaceSubscription {
    initial_epoch: NamespaceEpoch,
    receiver: broadcast::Receiver<NamespaceEvent>,
    hub: Arc<NamespaceEventHub>,
}

impl NamespaceSubscription {
    #[must_use]
    pub const fn initial_epoch(&self) -> NamespaceEpoch {
        self.initial_epoch
    }

    pub async fn recv(&mut self) -> Option<NamespaceEvent> {
        match self.receiver.recv().await {
            Ok(event) => Some(event),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                Some(NamespaceEvent::reset(self.hub.current_epoch()))
            },
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    pub fn try_recv(&mut self) -> Option<NamespaceEvent> {
        match self.receiver.try_recv() {
            Ok(event) => Some(event),
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                Some(NamespaceEvent::reset(self.hub.current_epoch()))
            },
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                None
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_orders_epoch_and_reset() {
        let first = NamespaceEpoch::initial([1; 16]);
        let second = first.next().unwrap();
        let hub = NamespaceEventHub::new(first, 4);
        let mut subscription = hub.subscribe();

        hub.advance(second).unwrap();
        let reset = subscription.try_recv().unwrap();
        assert_eq!(reset.epoch(), second);
        assert_eq!(reset.into_event(), NsEvent::reset());
        assert_eq!(hub.current_epoch(), second);
        assert!(hub.advance(first).is_err());
    }

    #[test]
    fn stale_generation_events_are_fenced() {
        let first = NamespaceEpoch::initial([1; 16]);
        let second = first.next().unwrap();
        let hub = NamespaceEventHub::new(first, 4);
        hub.advance(second).unwrap();
        assert!(!hub.publish_if_current(first, NsEvent::reset()));
        assert!(hub.publish_if_current(second, NsEvent::reset()));
    }
}
