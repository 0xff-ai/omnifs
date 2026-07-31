use omnifs_core::{
    ActionId, AttachmentSpec, AttachmentVersion, ResourceKey, ResourceKind, ResourceName,
    ResourceRevision,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::PathBuf;

/// Desired exposure of the complete shared namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentDefinition {
    pub name: ResourceName,
    pub spec: AttachmentSpec,
}

impl AttachmentDefinition {
    #[must_use]
    pub const fn name(&self) -> &ResourceName {
        &self.name
    }

    #[must_use]
    pub fn key(&self) -> ResourceKey {
        ResourceKey::new(ResourceKind::Attachment, self.name.clone())
    }
}

/// Durable observed lifecycle phase for one desired attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentPhase {
    Pending,
    WaitingForNamespace,
    Starting,
    Ready,
    Stopping,
    Retrying,
    Failed,
    Deleting,
}

/// Desired and observed facts for one daemon-owned attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentStatus {
    pub definition: AttachmentDefinition,
    pub desired_revision: ResourceRevision,
    pub desired_version: AttachmentVersion,
    pub observed_version: Option<AttachmentVersion>,
    pub phase: AttachmentPhase,
    pub runtime_instance: Option<String>,
    pub action_generation: u64,
    pub error_code: Option<String>,
    pub detail: Option<String>,
    pub retry_at_unix_ms: Option<u64>,
    pub deleting: bool,
}

/// Durable, reply-loss-safe request to restart one desired attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestartAttachmentRequest {
    pub action_id: ActionId,
    pub base_action_generation: u64,
    pub attachment: ResourceName,
}

/// Inputs used only to construct a typed shell or command invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetAttachmentAccessRequest {
    pub attachment: ResourceName,
    pub interactive: bool,
    pub shell: Option<String>,
    pub command: Vec<String>,
}

/// Exact argv returned by the daemon. Callers must execute it without shell
/// evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
}

/// Verified access to one ready attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentAccess {
    HostPath(PathBuf),
    Command(AttachmentCommand),
}
