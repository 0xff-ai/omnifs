use omnifs_core::{
    ActionId, FilesystemSpec, FilesystemVersion, ResourceKey, ResourceKind, ResourceName,
    ResourceRevision,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::PathBuf;

/// Desired exposure of the complete shared namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemDefinition {
    pub name: ResourceName,
    pub spec: FilesystemSpec,
}

impl FilesystemDefinition {
    #[must_use]
    pub const fn name(&self) -> &ResourceName {
        &self.name
    }

    #[must_use]
    pub fn key(&self) -> ResourceKey {
        ResourceKey::new(ResourceKind::Filesystem, self.name.clone())
    }
}

/// Durable observed lifecycle phase for one desired filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemPhase {
    Pending,
    WaitingForNamespace,
    Starting,
    Ready,
    Stopping,
    Retrying,
    Failed,
    Deleting,
}

/// Desired and observed facts for one daemon-owned filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilesystemStatus {
    pub definition: FilesystemDefinition,
    pub desired_revision: ResourceRevision,
    pub desired_version: FilesystemVersion,
    pub observed_version: Option<FilesystemVersion>,
    pub phase: FilesystemPhase,
    pub runtime_instance: Option<String>,
    pub action_generation: u64,
    pub error_code: Option<String>,
    pub detail: Option<String>,
    pub retry_at_unix_ms: Option<u64>,
    pub deleting: bool,
}

/// Durable, reply-loss-safe request to restart one desired filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestartFilesystemRequest {
    pub action_id: ActionId,
    pub base_action_generation: u64,
    pub filesystem: ResourceName,
}

/// Inputs used only to construct a typed shell or command invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetFilesystemAccessRequest {
    pub filesystem: ResourceName,
    pub interactive: bool,
    pub shell: Option<String>,
    pub command: Vec<String>,
}

/// Exact argv returned by the daemon. Callers must execute it without shell
/// evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilesystemCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
}

/// Verified access to one ready filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemAccess {
    HostPath(PathBuf),
    Command(FilesystemCommand),
}
