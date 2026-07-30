//! Typed domain values and bounds for the local tonic/protobuf control API.

use crate::{
    CredentialKey, CredentialStatus, CredentialSubmission, MountDefinition, MountOpResult,
    MountPatch,
};
use omnifs_core::{MountName, ProviderId};
use serde::{Deserialize, Serialize};
/// Limit for one unary protobuf message.
pub const CONTROL_MESSAGE_MAX_BYTES: usize = 1024 * 1024;
/// Limit for each item on a control stream.
pub const CONTROL_STREAM_ITEM_MAX_BYTES: usize = 1024 * 1024;
/// Payload budget after reserving protobuf envelope overhead.
pub const CONTROL_STREAM_PAYLOAD_MAX_BYTES: usize = CONTROL_STREAM_ITEM_MAX_BYTES - 32;
/// Maximum number of log lines that one stream request may ask for.
pub const CONTROL_LOG_TAIL_MAX_LINES: u32 = 10_000;
/// Deadline for one finite request, covering connect, write, and reply body.
pub const CONTROL_REQUEST_TIMEOUT_SECS: u64 = 5;
/// Deadline for a mutation that may prepare and drain a serving generation.
pub const CONTROL_MUTATION_TIMEOUT_SECS: u64 = 30;
/// Bound for the daemon's filesystem drain during shutdown.
pub const CONTROL_SHUTDOWN_DRAIN_SECS: u64 = 10;
/// Deadline for shutdown, which includes the daemon's bounded filesystem drain.
pub const CONTROL_SHUTDOWN_TIMEOUT_SECS: u64 = 15;

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
    /// Another mutation batch already holds the daemon's single lease.
    /// The message carries the holder's id and lease deadline for now,
    /// since the wire error envelope carries no structured detail channel.
    MutationInProgress,
    /// The caller's lease deadline passed before `ApplyMutation` reached the
    /// daemon.
    LeaseExpired,
    /// `ApplyMutation` or `DropMutation` named a mutation id that is not the
    /// current lease holder.
    LeaseNotHeld,
    NotFound,
    AlreadyExists,
    Conflict,
    Internal,
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

/// One typed op inside an `ApplyMutation` batch. `CredentialSubmission`
/// carries secret material, so this enum gets a hand-redacted `Debug` and
/// no `PartialEq`/`Eq` derive (mirrors `CredentialSubmission` itself).
#[derive(Serialize, Deserialize)]
pub enum MutationOp {
    CreateMount(MountDefinition),
    UpdateMount { name: MountName, patch: MountPatch },
    RemoveMount { name: MountName },
    SubmitCredential(CredentialSubmission),
    DeleteCredential(CredentialKey),
    RevokeCredential(CredentialKey),
}

impl std::fmt::Debug for MutationOp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateMount(definition) => formatter
                .debug_tuple("CreateMount")
                .field(definition)
                .finish(),
            Self::UpdateMount { name, patch } => formatter
                .debug_struct("UpdateMount")
                .field("name", name)
                .field("patch", patch)
                .finish(),
            Self::RemoveMount { name } => formatter
                .debug_struct("RemoveMount")
                .field("name", name)
                .finish(),
            Self::SubmitCredential(submission) => formatter
                .debug_tuple("SubmitCredential")
                .field(submission)
                .finish(),
            Self::DeleteCredential(key) => formatter
                .debug_tuple("DeleteCredential")
                .field(key)
                .finish(),
            Self::RevokeCredential(key) => formatter
                .debug_tuple("RevokeCredential")
                .field(key)
                .finish(),
        }
    }
}

/// Result of one op inside an applied mutation batch, in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationOpResult {
    Mount(MountOpResult),
    Credential(CredentialStatus),
}

/// Whether the daemon's serving generation reflects the just-applied batch.
/// Modeled on the recovery vocabulary in `DaemonRecovery`/`HealthReport`:
/// a plain flag plus an optional human detail, since nothing richer than
/// that exists yet for reporting serving state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServingOutcome {
    pub serving: bool,
    pub recovery_detail: Option<String>,
}
