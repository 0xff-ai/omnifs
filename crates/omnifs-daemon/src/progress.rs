//! Bounded, non-blocking progress fanout with complete snapshot recovery.

use omnifs_api::{
    ActionPhase, CredentialProgress, FilesystemProgress, ProgressEvent, ProgressEventKind,
    ProgressSnapshot, ProgressTarget, ProviderPreparationProgress, ResourcePhase, ServingProgress,
};
use omnifs_core::ResourceRevision;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

const LIVE_EVENT_CAPACITY: usize = 32;
const SUBSCRIBER_CAPACITY: usize = 8;

struct HubState {
    sequence: u64,
    snapshot: ProgressSnapshot,
    action_serving: HashMap<omnifs_core::ActionId, ServingProgress>,
    /// Exact provider membership for each desired revision. The aliases are
    /// authoritative for that revision and must not be inferred from the
    /// mutable current snapshot.
    revision_providers: BTreeMap<
        ResourceRevision,
        HashMap<omnifs_core::ProviderId, Vec<omnifs_core::ResourceName>>,
    >,
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
                action_serving: HashMap::new(),
                revision_providers: BTreeMap::new(),
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

    /// Mutate the complete snapshot under the hub lock, then publish the
    /// resulting snapshot. This avoids read-modify-write races between apply,
    /// action, and reconciliation owners.
    pub(crate) fn update_snapshot(
        &self,
        target: ProgressTarget,
        update: impl FnOnce(&mut ProgressSnapshot),
    ) -> u64 {
        let event = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            update(&mut state.snapshot);
            state.sequence = state
                .sequence
                .checked_add(1)
                .expect("daemon progress sequence exhausted");
            ProgressEvent {
                daemon_instance_id: self.daemon_instance_id.to_string(),
                sequence: state.sequence,
                target,
                event: ProgressEventKind::Snapshot(state.snapshot.clone()),
            }
        };
        let sequence = event.sequence;
        let _ = self.live.send(event);
        sequence
    }

    /// Update one desired revision only while it remains current.
    pub(crate) fn update_revision_snapshot(
        &self,
        revision: ResourceRevision,
        update: impl FnOnce(&mut ProgressSnapshot),
    ) -> bool {
        let event = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.snapshot.desired_revision != revision {
                return false;
            }
            update(&mut state.snapshot);
            state.sequence = state
                .sequence
                .checked_add(1)
                .expect("daemon progress sequence exhausted");
            ProgressEvent {
                daemon_instance_id: self.daemon_instance_id.to_string(),
                sequence: state.sequence,
                target: ProgressTarget::DesiredRevision(revision),
                event: ProgressEventKind::Snapshot(state.snapshot.clone()),
            }
        };
        let _ = self.live.send(event);
        true
    }

    /// Publish a typed transient stage. Broadcast send never waits and lagging
    /// receivers recover from the latest complete snapshot.
    pub(crate) fn publish(&self, target: ProgressTarget, event: ProgressEventKind) -> u64 {
        self.publish_inner(target, event, None)
    }

    /// Register the exact provider aliases used by one desired revision.
    /// Reconciliation calls this after durable desired state is accepted and
    /// before provider progress is exposed to revision subscribers.
    pub(crate) fn register_revision_providers(
        &self,
        revision: ResourceRevision,
        mut providers: HashMap<omnifs_core::ProviderId, Vec<omnifs_core::ResourceName>>,
    ) {
        for names in providers.values_mut() {
            names.sort();
            names.dedup();
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.revision_providers.insert(revision, providers);
        // Drop mappings that can no longer be requested. This also bounds
        // memory when a profile receives many applies over its lifetime.
        let current = state.snapshot.desired_revision;
        state
            .revision_providers
            .retain(|known, _| *known >= current);
    }

    /// Record provider status once, then fan it out to Current and every
    /// desired revision that names the exact digest. Revision events carry
    /// only that revision's aliases.
    pub(crate) fn record_provider_status(
        &self,
        progress: &ProviderPreparationProgress,
    ) -> Vec<u64> {
        let mut events = Vec::new();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update_provider_snapshot(&mut state.snapshot, progress.clone());
        let mut targets = vec![(ProgressTarget::Current, progress.resource_names.clone())];
        for (revision, providers) in &state.revision_providers {
            if let Some(names) = providers.get(&progress.digest) {
                targets.push((ProgressTarget::DesiredRevision(*revision), names.clone()));
            }
        }
        for (target, names) in targets {
            let mut payload = progress.clone();
            payload.resource_names = names;
            state.sequence = state
                .sequence
                .checked_add(1)
                .expect("daemon progress sequence exhausted");
            let event = ProgressEvent {
                daemon_instance_id: self.daemon_instance_id.to_string(),
                sequence: state.sequence,
                target,
                event: ProgressEventKind::ProviderPreparation(payload),
            };
            events.push(event.sequence);
            let _ = self.live.send(event);
        }
        events
    }

    pub(crate) fn record_serving_for_revision(
        &self,
        revision: ResourceRevision,
        progress: ServingProgress,
    ) -> u64 {
        let event = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.snapshot.desired_revision == revision {
                state.snapshot.serving = Some(progress.clone());
            }
            state.sequence = state
                .sequence
                .checked_add(1)
                .expect("daemon progress sequence exhausted");
            ProgressEvent {
                daemon_instance_id: self.daemon_instance_id.to_string(),
                sequence: state.sequence,
                target: ProgressTarget::DesiredRevision(revision),
                event: ProgressEventKind::ServingProgress(progress),
            }
        };
        let sequence = event.sequence;
        let _ = self.live.send(event);
        sequence
    }

    pub(crate) fn record_action_serving(
        &self,
        action_id: omnifs_core::ActionId,
        progress: ServingProgress,
    ) -> u64 {
        let event = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.action_serving.insert(action_id, progress.clone());
            state.sequence = state
                .sequence
                .checked_add(1)
                .expect("daemon progress sequence exhausted");
            ProgressEvent {
                daemon_instance_id: self.daemon_instance_id.to_string(),
                sequence: state.sequence,
                target: ProgressTarget::Action(action_id),
                event: ProgressEventKind::ServingProgress(progress),
            }
        };
        let sequence = event.sequence;
        let _ = self.live.send(event);
        sequence
    }

    pub(crate) fn record_action_receipt(&self, receipt: omnifs_api::ActionReceipt) -> u64 {
        let action_id = receipt.action_id;
        self.update_snapshot(ProgressTarget::Action(action_id), move |snapshot| {
            snapshot
                .actions
                .retain(|current| current.action_id != action_id);
            snapshot.actions.push(receipt);
            snapshot
                .actions
                .sort_by_key(|current| *current.action_id.as_bytes());
        })
    }

    pub(crate) fn record_credential(
        &self,
        target: ProgressTarget,
        progress: CredentialProgress,
    ) -> u64 {
        let event = ProgressEventKind::CredentialProgress(progress.clone());
        self.record(target, event, move |snapshot| {
            snapshot
                .credentials
                .retain(|current| current.key != progress.key);
            snapshot.credentials.push(progress);
            snapshot
                .credentials
                .sort_by(|left, right| left.key.cmp(&right.key));
        })
    }

    pub(crate) fn record_filesystem(
        &self,
        target: ProgressTarget,
        progress: FilesystemProgress,
    ) -> u64 {
        let event = ProgressEventKind::FilesystemProgress(progress.clone());
        self.record(target, event, move |snapshot| {
            snapshot
                .filesystems
                .retain(|current| current.key != progress.key);
            snapshot.filesystems.push(progress);
            snapshot
                .filesystems
                .sort_by(|left, right| left.key.cmp(&right.key));
        })
    }

    fn record(
        &self,
        target: ProgressTarget,
        event: ProgressEventKind,
        update: impl FnOnce(&mut ProgressSnapshot),
    ) -> u64 {
        let event = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            update(&mut state.snapshot);
            state.sequence = state
                .sequence
                .checked_add(1)
                .expect("daemon progress sequence exhausted");
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
        let initial_terminal = target_is_terminal(&snapshot, target);
        let initial = ProgressEvent {
            daemon_instance_id: self.daemon_instance_id.to_string(),
            sequence: watermark,
            target,
            event: ProgressEventKind::Snapshot(snapshot),
        };
        let (send, receive) = mpsc::channel(SUBSCRIBER_CAPACITY);
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            forward_subscription(
                hub,
                target,
                live,
                watermark,
                initial,
                initial_terminal,
                send,
            )
            .await;
        });
        receive
    }

    pub(crate) fn snapshot_for(&self, target: ProgressTarget) -> (u64, ProgressSnapshot) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            state.sequence,
            filter_snapshot(
                &state.snapshot,
                target,
                &state.revision_providers,
                &state.action_serving,
            ),
        )
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
    initial_terminal: bool,
    send: mpsc::Sender<ProgressEvent>,
) {
    if send.send(initial).await.is_err() {
        return;
    }
    if initial_terminal {
        return;
    }
    loop {
        match live.recv().await {
            Ok(mut event) => {
                if event.sequence <= watermark || !target_accepts(target, event.target) {
                    continue;
                }
                watermark = event.sequence;
                filter_event_snapshot(&hub, &mut event, target);
                let terminal = event_is_terminal(&event, target);
                if send.send(event).await.is_err() {
                    return;
                }
                if terminal {
                    return;
                }
            },
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let (next_watermark, snapshot) = hub.snapshot_for(target);
                watermark = next_watermark;
                let terminal = target_is_terminal(&snapshot, target);
                let resync = ProgressEvent {
                    daemon_instance_id: hub.daemon_instance_id.to_string(),
                    sequence: watermark,
                    target,
                    event: ProgressEventKind::Resync(snapshot),
                };
                if send.send(resync).await.is_err() {
                    return;
                }
                if terminal {
                    return;
                }
            },
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn target_is_terminal(snapshot: &ProgressSnapshot, target: ProgressTarget) -> bool {
    matches!(
        target_state(snapshot, target),
        ProgressTargetState::Ready | ProgressTargetState::Failed | ProgressTargetState::Superseded
    )
}

fn filter_event_snapshot(hub: &ProgressHub, event: &mut ProgressEvent, target: ProgressTarget) {
    match &mut event.event {
        ProgressEventKind::Snapshot(snapshot) | ProgressEventKind::Resync(snapshot) => {
            *snapshot = hub.snapshot_for(target).1;
        },
        _ => {},
    }
}

fn event_is_terminal(event: &ProgressEvent, target: ProgressTarget) -> bool {
    if event.target != target {
        return false;
    }
    matches!(
        (target, &event.event),
        (
            ProgressTarget::DesiredRevision(_),
            ProgressEventKind::RevisionReady(_)
                | ProgressEventKind::RevisionFailed { .. }
                | ProgressEventKind::RevisionSuperseded { .. }
        ) | (
            ProgressTarget::Action(_),
            ProgressEventKind::ActionCompleted(_) | ProgressEventKind::ActionFailed { .. }
        )
    )
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

fn filter_snapshot(
    snapshot: &ProgressSnapshot,
    target: ProgressTarget,
    revision_providers: &BTreeMap<
        ResourceRevision,
        HashMap<omnifs_core::ProviderId, Vec<omnifs_core::ResourceName>>,
    >,
    action_serving: &HashMap<omnifs_core::ActionId, ServingProgress>,
) -> ProgressSnapshot {
    let mut filtered = snapshot.clone();
    match target {
        ProgressTarget::Current => {},
        ProgressTarget::DesiredRevision(revision) => {
            filtered
                .resources
                .retain(|status| status.desired_revision == revision);
            filtered.actions.clear();
            let resource_names: std::collections::BTreeSet<_> = filtered
                .resources
                .iter()
                .map(|status| status.key.name.clone())
                .collect();
            let memberships = revision_providers.get(&revision);
            filtered.providers.retain_mut(|provider| {
                let Some(names) = memberships.and_then(|providers| providers.get(&provider.digest))
                else {
                    return false;
                };
                provider.resource_names = names.clone();
                true
            });
            if filtered
                .serving
                .as_ref()
                .is_some_and(|serving| serving.revision != revision)
            {
                filtered.serving = None;
            }
            filtered
                .credentials
                .retain(|progress| resource_names.contains(&progress.key.name));
            filtered
                .filesystems
                .retain(|progress| resource_names.contains(&progress.key.name));
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
            filtered.providers.clear();
            filtered.serving = action_serving.get(&action_id).cloned();
            filtered
                .credentials
                .retain(|progress| affected.as_ref() == Some(&progress.key));
            filtered
                .filesystems
                .retain(|progress| affected.as_ref() == Some(&progress.key));
        },
    }
    filtered
}

fn update_provider_snapshot(
    snapshot: &mut ProgressSnapshot,
    progress: ProviderPreparationProgress,
) {
    snapshot
        .providers
        .retain(|current| current.digest != progress.digest);
    snapshot.providers.push(progress);
    let common = snapshot
        .providers
        .last()
        .cloned()
        .expect("provider inserted");
    for current in &mut snapshot.providers {
        current.queued_digests = common.queued_digests;
        current.active_digests = common.active_digests;
        current.completed_digests = common.completed_digests;
    }
    snapshot
        .providers
        .sort_by_key(|current| *current.digest.as_bytes());
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
            if snapshot.serving.as_ref().is_some_and(|serving| {
                serving.revision == revision
                    && serving.stage == omnifs_api::ServingProgressStage::Failed
            }) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_api::{ActionKind, ActionReceipt, ProviderPreparationStage, ResourceStatus};
    use omnifs_core::{ActionId, ResourceKey, ResourceKind, ResourceName, ResourceRevision};
    use std::time::Duration;

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
            providers: Vec::new(),
            serving: None,
            credentials: Vec::new(),
            filesystems: Vec::new(),
        }
    }

    fn serving(revision: u64, stage: omnifs_api::ServingProgressStage) -> ServingProgress {
        ServingProgress {
            revision: ResourceRevision::new(revision),
            stage,
            completed: 0,
            total: 1,
            error_code: None,
            detail: None,
            queued_generations: 0,
            retry_count: 0,
            next_retry_unix_ms: None,
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
                ProgressEventKind::ServingProgress(serving(
                    1,
                    omnifs_api::ServingProgressStage::Building,
                )),
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
                ProgressEventKind::ServingProgress(serving(
                    1,
                    omnifs_api::ServingProgressStage::Building,
                )),
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
            error_code: None,
            detail: None,
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
            ProgressEventKind::ServingProgress(serving(
                2,
                omnifs_api::ServingProgressStage::Building,
            )),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), revision.recv())
                .await
                .is_ok_and(|event| event.is_none())
        );
    }

    #[tokio::test]
    async fn revision_provider_membership_is_exact_and_streams_close_at_terminal_event() {
        let digest = omnifs_core::ProviderId::from_wasm_bytes(b"provider");
        let mut current = snapshot(3, ResourcePhase::Pending);
        current.providers.push(ProviderPreparationProgress {
            digest,
            catalog_name: "catalog".into(),
            resource_names: vec![name("old"), name("new")],
            stage: ProviderPreparationStage::Ready,
            completed_bytes: 1,
            total_bytes: Some(1),
            error_code: None,
            detail: None,
            queued_digests: 0,
            active_digests: 0,
            queue_position: None,
            completed_digests: 1,
            retry_count: 0,
        });
        let hub = ProgressHub::new("daemon", current);
        let mut memberships = HashMap::new();
        memberships.insert(digest, vec![name("new")]);
        hub.register_revision_providers(ResourceRevision::new(3), memberships);
        let (_, filtered) =
            hub.snapshot_for(ProgressTarget::DesiredRevision(ResourceRevision::new(3)));
        assert_eq!(filtered.providers[0].resource_names, vec![name("new")]);

        let mut stream = hub.subscribe(ProgressTarget::DesiredRevision(ResourceRevision::new(3)));
        assert!(stream.recv().await.is_some());
        hub.record_provider_status(&ProviderPreparationProgress {
            digest,
            catalog_name: "catalog".into(),
            resource_names: vec![name("old"), name("new")],
            stage: ProviderPreparationStage::Compiling,
            completed_bytes: 0,
            total_bytes: Some(1),
            error_code: None,
            detail: None,
            queued_digests: 0,
            active_digests: 1,
            queue_position: Some(1),
            completed_digests: 0,
            retry_count: 0,
        });
        let provider_event = stream.recv().await.unwrap();
        match provider_event.event {
            ProgressEventKind::ProviderPreparation(progress) => {
                assert_eq!(progress.resource_names, vec![name("new")]);
            },
            other => panic!("unexpected event: {other:?}"),
        }
        hub.publish(
            ProgressTarget::DesiredRevision(ResourceRevision::new(3)),
            ProgressEventKind::RevisionReady(ResourceRevision::new(3)),
        );
        assert!(stream.recv().await.is_some());
        assert!(stream.recv().await.is_none());
    }

    #[test]
    fn stale_revision_updates_never_replace_the_current_snapshot() {
        let hub = ProgressHub::new("daemon", snapshot(1, ResourcePhase::Preparing));
        hub.update_snapshot(
            ProgressTarget::DesiredRevision(ResourceRevision::new(2)),
            |current| {
                *current = snapshot(2, ResourcePhase::Pending);
            },
        );

        hub.record_serving_for_revision(
            ResourceRevision::new(1),
            serving(1, omnifs_api::ServingProgressStage::Ready),
        );
        assert!(
            !hub.update_revision_snapshot(ResourceRevision::new(1), |current| {
                current.observed_revision = Some(ResourceRevision::new(1));
            })
        );

        let (_, current) = hub.snapshot_for(ProgressTarget::Current);
        assert_eq!(current.desired_revision, ResourceRevision::new(2));
        assert_eq!(current.resources, vec![resource(2, ResourcePhase::Pending)]);
        assert!(current.serving.is_none());
        assert!(current.observed_revision.is_none());
    }

    #[test]
    fn action_snapshot_keeps_its_correlated_serving_stage_across_reconnect() {
        let action_id = ActionId::from_bytes([9; 16]);
        let mut current = snapshot(1, ResourcePhase::Pending);
        current.actions.push(ActionReceipt {
            action_id,
            kind: ActionKind::SetCredentialMaterial,
            target: ResourceKey::new(ResourceKind::Credential, name("account")),
            action_generation: 3,
            phase: ActionPhase::Running,
            error_code: None,
            detail: None,
        });
        let hub = ProgressHub::new("daemon", current);
        let progress = serving(1, omnifs_api::ServingProgressStage::Building);
        hub.record_action_serving(action_id, progress.clone());

        let (_, running) = hub.snapshot_for(ProgressTarget::Action(action_id));
        assert_eq!(running.serving, Some(progress.clone()));
        let mut ready = running.actions[0].clone();
        ready.phase = ActionPhase::Ready;
        hub.record_action_receipt(ready);

        let (_, reconnected) = hub.snapshot_for(ProgressTarget::Action(action_id));
        assert_eq!(reconnected.actions[0].phase, ActionPhase::Ready);
        assert_eq!(reconnected.serving, Some(progress));
        assert_eq!(
            hub.target_state(ProgressTarget::Action(action_id)),
            ProgressTargetState::Ready
        );
    }
}
