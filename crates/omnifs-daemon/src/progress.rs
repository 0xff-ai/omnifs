//! Bounded, non-blocking progress fanout with complete snapshot recovery.

use omnifs_api::{
    ActionPhase, ProgressEvent, ProgressEventKind, ProgressSnapshot, ProgressTarget, ResourcePhase,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

const LIVE_EVENT_CAPACITY: usize = 32;
const SUBSCRIBER_CAPACITY: usize = 8;
#[allow(dead_code, reason = "Plan 003 provider work uses this rate")]
pub(crate) const BYTE_PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);

struct HubState {
    sequence: u64,
    snapshot: ProgressSnapshot,
}

/// One daemon-instance progress owner. Durable state remains in `SQLite`.
pub(crate) struct ProgressHub {
    daemon_instance_id: Arc<str>,
    state: Mutex<HubState>,
    live: broadcast::Sender<ProgressEvent>,
}

impl ProgressHub {
    pub(crate) fn new(
        daemon_instance_id: impl Into<Arc<str>>,
        snapshot: ProgressSnapshot,
    ) -> Arc<Self> {
        let (live, _) = broadcast::channel(LIVE_EVENT_CAPACITY);
        Arc::new(Self {
            daemon_instance_id: daemon_instance_id.into(),
            state: Mutex::new(HubState {
                sequence: 1,
                snapshot,
            }),
            live,
        })
    }

    /// Replace the complete snapshot and publish it without waiting for a
    /// subscriber. Reconcile owners call this after durable state changes.
    pub(crate) fn publish_snapshot(
        &self,
        target: ProgressTarget,
        snapshot: ProgressSnapshot,
    ) -> u64 {
        self.publish_inner(
            target,
            ProgressEventKind::Snapshot(snapshot.clone()),
            Some(snapshot),
        )
    }

    /// Publish a typed transient stage. Broadcast send never waits and lagging
    /// receivers recover from the latest complete snapshot.
    #[allow(dead_code, reason = "Plan 003 reconciliation publishes live stages")]
    pub(crate) fn publish(&self, target: ProgressTarget, event: ProgressEventKind) -> u64 {
        self.publish_inner(target, event, None)
    }

    fn publish_inner(
        &self,
        target: ProgressTarget,
        event: ProgressEventKind,
        snapshot: Option<ProgressSnapshot>,
    ) -> u64 {
        let event = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.sequence = state
                .sequence
                .checked_add(1)
                .expect("daemon progress sequence exhausted");
            if let Some(snapshot) = snapshot {
                state.snapshot = snapshot;
            }
            ProgressEvent {
                daemon_instance_id: self.daemon_instance_id.to_string(),
                sequence: state.sequence,
                target,
                event,
            }
        };
        let sequence = event.sequence;
        let _ = self.live.send(event);
        sequence
    }

    /// Subscribe before reading the snapshot watermark. This order closes the
    /// subscribe-versus-update race without putting fanout on a daemon worker.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        target: ProgressTarget,
    ) -> mpsc::Receiver<ProgressEvent> {
        let live = self.live.subscribe();
        let (watermark, snapshot) = self.snapshot_for(target);
        let initial = ProgressEvent {
            daemon_instance_id: self.daemon_instance_id.to_string(),
            sequence: watermark,
            target,
            event: ProgressEventKind::Snapshot(snapshot),
        };
        let (send, receive) = mpsc::channel(SUBSCRIBER_CAPACITY);
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            forward_subscription(hub, target, live, watermark, initial, send).await;
        });
        receive
    }

    pub(crate) fn snapshot_for(&self, target: ProgressTarget) -> (u64, ProgressSnapshot) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.sequence, filter_snapshot(&state.snapshot, target))
    }

    pub(crate) fn target_state(&self, target: ProgressTarget) -> ProgressTargetState {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        target_state(&state.snapshot, target)
    }
}

async fn forward_subscription(
    hub: Arc<ProgressHub>,
    target: ProgressTarget,
    mut live: broadcast::Receiver<ProgressEvent>,
    mut watermark: u64,
    initial: ProgressEvent,
    send: mpsc::Sender<ProgressEvent>,
) {
    if send.send(initial).await.is_err() {
        return;
    }
    loop {
        match live.recv().await {
            Ok(mut event) => {
                if event.sequence <= watermark || !target_accepts(target, event.target) {
                    continue;
                }
                watermark = event.sequence;
                filter_event_snapshot(&mut event, target);
                if send.send(event).await.is_err() {
                    return;
                }
            },
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let (next_watermark, snapshot) = hub.snapshot_for(target);
                watermark = next_watermark;
                let resync = ProgressEvent {
                    daemon_instance_id: hub.daemon_instance_id.to_string(),
                    sequence: watermark,
                    target,
                    event: ProgressEventKind::Resync(snapshot),
                };
                if send.send(resync).await.is_err() {
                    return;
                }
            },
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn filter_event_snapshot(event: &mut ProgressEvent, target: ProgressTarget) {
    match &mut event.event {
        ProgressEventKind::Snapshot(snapshot) | ProgressEventKind::Resync(snapshot) => {
            *snapshot = filter_snapshot(snapshot, target);
        },
        _ => {},
    }
}

fn target_accepts(subscription: ProgressTarget, event: ProgressTarget) -> bool {
    match subscription {
        ProgressTarget::Current => true,
        ProgressTarget::DesiredRevision(revision) => {
            event == ProgressTarget::DesiredRevision(revision)
        },
        ProgressTarget::Action(action_id) => event == ProgressTarget::Action(action_id),
    }
}

fn filter_snapshot(snapshot: &ProgressSnapshot, target: ProgressTarget) -> ProgressSnapshot {
    let mut filtered = snapshot.clone();
    match target {
        ProgressTarget::Current => {},
        ProgressTarget::DesiredRevision(revision) => {
            filtered
                .resources
                .retain(|status| status.desired_revision == revision);
            filtered.actions.clear();
        },
        ProgressTarget::Action(action_id) => {
            filtered
                .actions
                .retain(|receipt| receipt.action_id == action_id);
            let affected = filtered
                .actions
                .first()
                .map(|receipt| receipt.target.clone());
            filtered
                .resources
                .retain(|status| affected.as_ref() == Some(&status.key));
        },
    }
    filtered
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressTargetState {
    Watching,
    Ready,
    Failed,
    Superseded,
    Current,
    Unavailable,
}

fn target_state(snapshot: &ProgressSnapshot, target: ProgressTarget) -> ProgressTargetState {
    match target {
        ProgressTarget::Current => ProgressTargetState::Current,
        ProgressTarget::Action(action_id) => snapshot
            .actions
            .iter()
            .find(|receipt| receipt.action_id == action_id)
            .map_or(ProgressTargetState::Unavailable, |receipt| {
                match receipt.phase {
                    ActionPhase::Ready => ProgressTargetState::Ready,
                    ActionPhase::Failed => ProgressTargetState::Failed,
                    ActionPhase::Accepted | ActionPhase::Running | ActionPhase::Retrying => {
                        ProgressTargetState::Watching
                    },
                }
            }),
        ProgressTarget::DesiredRevision(revision) => {
            if snapshot.desired_revision > revision {
                return ProgressTargetState::Superseded;
            }
            if snapshot.desired_revision < revision {
                return ProgressTargetState::Unavailable;
            }
            if snapshot
                .resources
                .iter()
                .filter(|status| status.desired_revision == revision)
                .any(|status| {
                    matches!(status.phase, ResourcePhase::Failed | ResourcePhase::Blocked)
                })
            {
                return ProgressTargetState::Failed;
            }
            if snapshot
                .observed_revision
                .is_some_and(|observed| observed >= revision)
                && snapshot
                    .resources
                    .iter()
                    .filter(|status| status.desired_revision == revision)
                    .all(|status| status.phase == ResourcePhase::Ready)
            {
                ProgressTargetState::Ready
            } else {
                ProgressTargetState::Watching
            }
        },
    }
}

/// Rate gate for high-frequency real byte counts. Terminal counts always pass.
#[allow(dead_code, reason = "Plan 003 provider work uses this rate gate")]
pub(crate) struct ByteProgressGate {
    last_published: Option<Instant>,
}

#[allow(dead_code, reason = "Plan 003 provider work uses this rate gate")]
impl ByteProgressGate {
    pub(crate) const fn new() -> Self {
        Self {
            last_published: None,
        }
    }

    pub(crate) fn should_publish(&mut self, now: Instant, terminal: bool) -> bool {
        if terminal
            || self
                .last_published
                .is_none_or(|last| now.duration_since(last) >= BYTE_PROGRESS_MIN_INTERVAL)
        {
            self.last_published = Some(now);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::{ActionKind, ActionReceipt, ResourceStatus};
    use omnifs_core::{ActionId, ResourceKey, ResourceKind, ResourceName, ResourceRevision};

    fn name(value: &str) -> ResourceName {
        ResourceName::new(value).unwrap()
    }

    fn resource(revision: u64, phase: ResourcePhase) -> ResourceStatus {
        ResourceStatus {
            key: ResourceKey::new(ResourceKind::Provider, name("demo")),
            desired_revision: ResourceRevision::new(revision),
            observed_revision: (phase == ResourcePhase::Ready)
                .then_some(ResourceRevision::new(revision)),
            phase,
            error_code: None,
            detail: None,
        }
    }

    fn snapshot(revision: u64, phase: ResourcePhase) -> ProgressSnapshot {
        ProgressSnapshot {
            desired_revision: ResourceRevision::new(revision),
            observed_revision: (phase == ResourcePhase::Ready)
                .then_some(ResourceRevision::new(revision)),
            resources: vec![resource(revision, phase)],
            actions: Vec::new(),
        }
    }

    #[tokio::test]
    async fn subscribe_then_snapshot_closes_the_update_race_and_sequences_events() {
        let hub = ProgressHub::new("daemon", snapshot(1, ResourcePhase::Pending));
        let mut receive = hub.subscribe(ProgressTarget::DesiredRevision(ResourceRevision::new(1)));
        let sequence = hub.publish_snapshot(
            ProgressTarget::DesiredRevision(ResourceRevision::new(1)),
            snapshot(1, ResourcePhase::Ready),
        );
        let first = receive.recv().await.unwrap();
        let second = receive.recv().await.unwrap();
        assert!(first.sequence < second.sequence);
        assert_eq!(second.sequence, sequence);
        assert!(matches!(second.event, ProgressEventKind::Snapshot(_)));
    }

    #[tokio::test]
    async fn lagged_consumers_resync_and_disconnect_never_blocks_publishers() {
        let hub = ProgressHub::new("daemon", snapshot(1, ResourcePhase::Pending));
        let mut receive = hub.subscribe(ProgressTarget::Current);
        for _ in 0..(LIVE_EVENT_CAPACITY * 4) {
            hub.publish(
                ProgressTarget::Current,
                ProgressEventKind::ResourcePhaseChanged(resource(1, ResourcePhase::Preparing)),
            );
        }
        let mut resynced = false;
        for _ in 0..(SUBSCRIBER_CAPACITY + 4) {
            let event = tokio::time::timeout(Duration::from_secs(1), receive.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(event.event, ProgressEventKind::Resync(_)) {
                resynced = true;
                break;
            }
        }
        assert!(resynced);
        drop(receive);
        for _ in 0..LIVE_EVENT_CAPACITY {
            hub.publish(
                ProgressTarget::Current,
                ProgressEventKind::ResourcePhaseChanged(resource(1, ResourcePhase::Preparing)),
            );
        }
    }

    #[tokio::test]
    async fn targets_filter_unrelated_work_and_snapshots_drive_terminal_state() {
        let action_id = ActionId::from_bytes([7; 16]);
        let mut complete = snapshot(2, ResourcePhase::Ready);
        complete.actions.push(ActionReceipt {
            action_id,
            kind: ActionKind::SetCredentialMaterial,
            target: ResourceKey::new(ResourceKind::Credential, name("account")),
            action_generation: 1,
            phase: ActionPhase::Ready,
        });
        let hub = ProgressHub::new("daemon", complete);
        assert_eq!(
            hub.target_state(ProgressTarget::DesiredRevision(ResourceRevision::new(2))),
            ProgressTargetState::Ready
        );
        assert_eq!(
            hub.target_state(ProgressTarget::DesiredRevision(ResourceRevision::new(1))),
            ProgressTargetState::Superseded
        );
        assert_eq!(
            hub.target_state(ProgressTarget::Action(action_id)),
            ProgressTargetState::Ready
        );
        assert_eq!(
            hub.target_state(ProgressTarget::Current),
            ProgressTargetState::Current
        );

        let mut revision = hub.subscribe(ProgressTarget::DesiredRevision(ResourceRevision::new(2)));
        let _ = revision.recv().await.unwrap();
        hub.publish(
            ProgressTarget::Current,
            ProgressEventKind::ResourcePhaseChanged(resource(2, ResourcePhase::Preparing)),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), revision.recv())
                .await
                .is_err()
        );
    }

    #[test]
    fn byte_progress_is_rate_limited_but_final_counts_always_publish() {
        let start = Instant::now();
        let mut gate = ByteProgressGate::new();
        assert!(gate.should_publish(start, false));
        assert!(!gate.should_publish(start + Duration::from_millis(10), false));
        assert!(gate.should_publish(start + BYTE_PROGRESS_MIN_INTERVAL, false));
        assert!(gate.should_publish(start + BYTE_PROGRESS_MIN_INTERVAL, true));
    }
}
