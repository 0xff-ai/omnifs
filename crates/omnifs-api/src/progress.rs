//! Closed, non-secret progress values for resource reconciliation and actions.

use crate::{ActionReceipt, ResourceStatus};
use omnifs_core::{ActionId, ProviderId, ResourceKey, ResourceName, ResourceRevision};
use serde::{Deserialize, Serialize};

/// A bounded target for one progress subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressTarget {
    DesiredRevision(ResourceRevision),
    Action(ActionId),
    Current,
}

/// The latest complete non-secret progress state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgressSnapshot {
    pub desired_revision: ResourceRevision,
    pub observed_revision: Option<ResourceRevision>,
    pub resources: Vec<ResourceStatus>,
    pub actions: Vec<ActionReceipt>,
    pub providers: Vec<ProviderPreparationProgress>,
    pub serving: Option<ServingProgress>,
    pub credentials: Vec<CredentialProgress>,
    pub attachments: Vec<AttachmentProgress>,
}

/// Closed provider-preparation stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderPreparationStage {
    Queued,
    Fetching,
    Validating,
    Compiling,
    Retrying,
    Ready,
    Failed,
}

/// Closed serving-generation stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServingProgressStage {
    Queued,
    WaitingProviders,
    ProvidersReady,
    Building,
    Built,
    Publishing,
    Draining,
    Degraded,
    Retrying,
    Superseded,
    Ready,
    Failed,
}

/// Closed credential-operation stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialProgressStage {
    Queued,
    Refreshing,
    Revoking,
    Ready,
    Failed,
}

/// Closed attachment-operation stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentProgressStage {
    Queued,
    Starting,
    Stopping,
    Ready,
    Failed,
}

/// Progress for one unique provider artifact digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderPreparationProgress {
    pub digest: ProviderId,
    pub catalog_name: String,
    pub resource_names: Vec<ResourceName>,
    pub stage: ProviderPreparationStage,
    pub completed_bytes: u64,
    pub total_bytes: Option<u64>,
    pub error_code: Option<String>,
    pub detail: Option<String>,
    pub queued_digests: u32,
    pub active_digests: u32,
    pub queue_position: Option<u32>,
    pub completed_digests: u32,
    pub retry_count: u32,
    pub next_retry_unix_ms: Option<u64>,
}

/// Progress for one desired serving generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServingProgress {
    pub revision: ResourceRevision,
    pub stage: ServingProgressStage,
    pub completed: u32,
    pub total: u32,
    pub error_code: Option<String>,
    pub detail: Option<String>,
    pub queued_generations: u32,
    pub retry_count: u32,
    pub next_retry_unix_ms: Option<u64>,
}

/// Progress for one credential resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialProgress {
    pub key: ResourceKey,
    pub stage: CredentialProgressStage,
    pub error_code: Option<String>,
    pub detail: Option<String>,
    pub retry_count: u32,
    pub next_retry_unix_ms: Option<u64>,
}

/// Progress for one attachment resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentProgress {
    pub key: ResourceKey,
    pub stage: AttachmentProgressStage,
    pub error_code: Option<String>,
    pub detail: Option<String>,
    pub retry_count: u32,
    pub next_retry_unix_ms: Option<u64>,
}

/// Strict event payloads. None carries credential material, configuration,
/// environment names, or local provider source paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEventKind {
    Snapshot(ProgressSnapshot),
    ResourcePhaseChanged(ResourceStatus),
    ProviderPreparation(ProviderPreparationProgress),
    ServingProgress(ServingProgress),
    CredentialProgress(CredentialProgress),
    AttachmentProgress(AttachmentProgress),
    RevisionReady(ResourceRevision),
    RevisionFailed {
        revision: ResourceRevision,
        error_code: String,
        detail: String,
    },
    RevisionSuperseded {
        revision: ResourceRevision,
        replaced_by: ResourceRevision,
    },
    ActionCompleted(ActionReceipt),
    ActionFailed {
        receipt: ActionReceipt,
        error_code: String,
        detail: String,
    },
    Resync(ProgressSnapshot),
}

/// A daemon-instance-scoped, monotonically sequenced progress event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgressEvent {
    pub daemon_instance_id: String,
    pub sequence: u64,
    pub target: ProgressTarget,
    pub event: ProgressEventKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActionKind, ActionPhase};
    use omnifs_core::{ResourceKind, ResourceName};

    #[test]
    fn action_progress_contains_only_its_non_secret_receipt() {
        let receipt = ActionReceipt {
            action_id: ActionId::from_bytes([1; 16]),
            kind: ActionKind::SetCredentialMaterial,
            target: ResourceKey::new(
                ResourceKind::Credential,
                ResourceName::new("github").unwrap(),
            ),
            action_generation: 1,
            phase: ActionPhase::Ready,
            error_code: None,
            detail: None,
        };
        let event = ProgressEvent {
            daemon_instance_id: "daemon".into(),
            sequence: 1,
            target: ProgressTarget::Action(receipt.action_id),
            event: ProgressEventKind::ActionCompleted(receipt),
        };
        let debug = format!("{event:?}");
        assert!(!debug.contains("token"));
        assert!(!debug.contains("secret"));
    }
}
