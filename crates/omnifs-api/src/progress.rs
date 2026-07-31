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
    pub filesystems: Vec<FilesystemProgress>,
}

/// Closed provider-preparation stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderPreparationStage {
    Queued,
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
    Refreshing,
    Revoking,
    Ready,
    Failed,
}

/// Closed filesystem-operation stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemProgressStage {
    Queued,
    WaitingForNamespace,
    PullingImage,
    Materializing,
    Starting,
    Mounting,
    Stopping,
    Retrying,
    Deleting,
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
}

/// Progress for one filesystem resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilesystemProgress {
    pub key: ResourceKey,
    pub desired_revision: ResourceRevision,
    pub runtime: omnifs_core::FilesystemRuntime,
    pub stage: FilesystemProgressStage,
    pub completed_bytes: u64,
    pub total_bytes: Option<u64>,
    pub queued_filesystems: u32,
    pub active_filesystems: u32,
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
    ProviderPreparation(ProviderPreparationProgress),
    ServingProgress(ServingProgress),
    CredentialProgress(CredentialProgress),
    FilesystemProgress(FilesystemProgress),
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
