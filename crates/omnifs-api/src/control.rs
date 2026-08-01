//! Typed domain values and bounds for the local tonic/protobuf control API.

use crate::{CredentialClientOverrides, CredentialMaterial, CredentialStatusKind};
use omnifs_core::{ActionId, ProviderId, ResourceKey, ResourceName};
use serde::{Deserialize, Serialize};
/// Limit for one unary protobuf message.
pub const CONTROL_MESSAGE_MAX_BYTES: usize = 1024 * 1024;
/// Limit for each item on a control stream.
pub const CONTROL_STREAM_ITEM_MAX_BYTES: usize = 1024 * 1024;
/// Payload budget after reserving protobuf envelope overhead.
pub const CONTROL_STREAM_PAYLOAD_MAX_BYTES: usize = CONTROL_STREAM_ITEM_MAX_BYTES - 32;
/// Maximum number of typed desired-resource declarations in one request or reply.
pub const CONTROL_RESOURCE_MAX_COUNT: usize = 1024;
/// Maximum number of log lines that one stream request may ask for.
pub const CONTROL_LOG_TAIL_MAX_LINES: u32 = 10_000;
/// Deadline for one finite request, covering connect, write, and reply body.
pub const CONTROL_REQUEST_TIMEOUT_SECS: u64 = 5;
/// Bound for the daemon's filesystem drain during shutdown.
pub const CONTROL_SHUTDOWN_DRAIN_SECS: u64 = 10;
/// Deadline for shutdown, which includes the daemon's bounded filesystem drain.
pub const CONTROL_SHUTDOWN_TIMEOUT_SECS: u64 = 15;
/// Deadline for one complete daemon doctor report or repair batch.
pub const CONTROL_DOCTOR_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlError {
    pub code: ControlErrorCode,
    pub message: String,
}

impl ControlError {
    #[must_use]
    pub fn new(code: ControlErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlErrorCode {
    Busy,
    NotReady,
    RecoveryRequired,
    InvalidRequest,
    NotFound,
    AlreadyExists,
    Conflict,
    UnsupportedApiVersion,
    InvalidResource,
    StaleBaseRevision,
    DesiredDigestMismatch,
    MutationIdReuseMismatch,
    MissingProviderArtifact,
    PlanTooLarge,
    ActionUnavailable,
    ActionIdReuseMismatch,
    Internal,
}

/// Closed durable operations that can outlive their control request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    SetCredentialMaterial,
    RevokeCredential,
    RestartFilesystem,
}

/// Current durable action phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionPhase {
    Accepted,
    Running,
    Retrying,
    Ready,
    Failed,
}

/// Non-secret durable receipt for a typed action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionReceipt {
    pub action_id: ActionId,
    pub kind: ActionKind,
    pub target: ResourceKey,
    pub action_generation: u64,
    pub phase: ActionPhase,
    pub error_code: Option<String>,
    pub detail: Option<String>,
}

/// A credential action request. Material and overrides remain request-only.
pub struct SetCredentialMaterialRequest {
    pub action_id: ActionId,
    pub base_action_generation: u64,
    pub credential: ResourceName,
    pub material: CredentialMaterial,
    pub overrides: CredentialClientOverrides,
}

impl std::fmt::Debug for SetCredentialMaterialRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SetCredentialMaterialRequest")
            .field("action_id", &self.action_id)
            .field("base_action_generation", &self.base_action_generation)
            .field("credential", &self.credential)
            .field("material", &self.material)
            .field("overrides", &self.overrides)
            .finish()
    }
}

/// Non-secret credential revocation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RevokeCredentialRequest {
    pub action_id: ActionId,
    pub base_action_generation: u64,
    pub credential: ResourceName,
}

/// Correlation receipt for a credential action, with only safe status facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialReceipt {
    pub action: ActionReceipt,
    pub status: CredentialStatusKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderImportDisposition {
    Inserted,
    Unchanged,
    Repaired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderImportReceipt {
    pub provider: ProviderReference,
    pub disposition: ProviderImportDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderReference {
    pub id: ProviderId,
    pub name: String,
    pub version: Option<String>,
}
