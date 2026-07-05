//! Control API types shared by the `omnifs` CLI and daemon runtime.
//!
//! The daemon serves these under `/v1/` on its control listener (TCP
//! loopback through the container port forward today; a Unix socket in the
//! future host-native mode). See `docs/contracts/50-control-plane.md`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use utoipa::ToSchema;

/// JSONL activity-event schema and redaction for the inspector observability
/// subsystem: the daemon emits [`events::InspectorRecord`] lines and the CLI
/// `inspect` command reads them. This is the events half of the same control-plane
/// wire contract the REST DTOs below describe.
pub mod events;

/// Control API major version. The CLI refuses to talk to a daemon with a
/// different major. Bump when routes or payloads change incompatibly.
pub const API_MAJOR: u16 = 3;

/// Control API minor version. The CLI warns but proceeds when the daemon's
/// minor differs. Bump for additive, backward-compatible additions.
pub const API_MINOR: u16 = 0;

/// Docker container name environment variable set by launchers and read by the
/// daemon when reporting backend identity.
pub const OMNIFS_CONTAINER_NAME_ENV: &str = "OMNIFS_CONTAINER_NAME";

/// Docker image environment variable set by launchers and read by the daemon
/// when reporting backend identity.
pub const OMNIFS_IMAGE_ENV: &str = "OMNIFS_IMAGE";

/// Default control port. The container publishes it on the host loopback;
/// both binaries default to it so `omnifs` finds the daemon with zero config.
pub const DEFAULT_PORT: u16 = 7878;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    AuthRequired,
    ConsentRequired,
    MountNotFound,
    SpecInvalid,
    ProviderMissing,
    ReconcileBusy,
    DaemonShuttingDown,
    Internal,
}

/// `GET /v1/ready`: 200 with `ready: true` once the filesystem is serving.
/// Non-ready responses use [`ApiError`].
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReadyInfo {
    pub ready: bool,
}

/// `GET /v1/status`: the daemon's runtime facts. Host-side state (mount
/// configs, credential readiness) is the CLI's contribution to the merged
/// status view; it never comes from the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DaemonStatus {
    pub version: String,
    /// Control API major version. Incompatible change when this differs.
    #[serde(default)]
    pub api_major: u16,
    /// Control API minor version. Additive change; CLI warns and proceeds.
    #[serde(default)]
    pub api_minor: u16,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    #[schema(value_type = String)]
    pub executable: PathBuf,
    #[schema(value_type = String)]
    pub mount_point: PathBuf,
    #[schema(value_type = String)]
    pub config_dir: PathBuf,
    #[schema(value_type = String)]
    pub cache_dir: PathBuf,
    #[schema(value_type = String)]
    pub providers_dir: PathBuf,
    /// The serving filesystem frontend (FUSE today; the protocol stays
    /// frontend-agnostic for future NFSv4/FSKit modes), when one is up.
    pub frontend: Option<FrontendInfo>,
    /// Backend serving this daemon, so the CLI tears down and reports the right
    /// backend without inferring it from configuration. Missing identity is not
    /// reclaimable; teardown stops instead of guessing.
    #[serde(default, alias = "launch")]
    pub backend: DaemonBackend,
    /// Provider mounts loaded in the registry.
    pub mounts: Vec<MountInfo>,
    /// Mounts that did not converge at the last reconcile, with reasons. Empty
    /// when every desired mount is serving; a dark mount appears here, not as a
    /// silent absence from `mounts`.
    #[serde(default)]
    pub failed: Vec<MountFailure>,
    /// Daemon-owned health for runtime subsystems. CLI status renders these
    /// entries instead of reconstructing daemon health from raw fields.
    #[serde(default)]
    pub health: DaemonHealth,
}

impl DaemonStatus {
    #[must_use]
    pub fn ready(&self) -> bool {
        self.health
            .frontend_ready()
            .unwrap_or_else(|| self.frontend.is_some())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct DaemonHealth {
    pub subsystems: Vec<SubsystemHealth>,
}

impl DaemonHealth {
    #[must_use]
    pub fn new(subsystems: Vec<SubsystemHealth>) -> Self {
        Self { subsystems }
    }

    #[must_use]
    pub fn subsystem(&self, subsystem: DaemonSubsystem) -> Option<&SubsystemHealth> {
        self.subsystems
            .iter()
            .find(|entry| entry.subsystem == subsystem)
    }

    #[must_use]
    pub fn frontend_ready(&self) -> Option<bool> {
        self.subsystem(DaemonSubsystem::Frontend)
            .map(|entry| entry.state == HealthState::Healthy)
    }

    #[must_use]
    pub fn overall_state(&self) -> HealthState {
        if self
            .subsystems
            .iter()
            .any(|entry| entry.state == HealthState::Unhealthy)
        {
            HealthState::Unhealthy
        } else if self
            .subsystems
            .iter()
            .any(|entry| entry.state == HealthState::Degraded)
        {
            HealthState::Degraded
        } else if self
            .subsystems
            .iter()
            .any(|entry| entry.state == HealthState::Starting)
        {
            HealthState::Starting
        } else {
            HealthState::Healthy
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SubsystemHealth {
    pub subsystem: DaemonSubsystem,
    pub state: HealthState,
    pub message: String,
}

impl SubsystemHealth {
    #[must_use]
    pub fn new(subsystem: DaemonSubsystem, state: HealthState, message: impl Into<String>) -> Self {
        Self {
            subsystem,
            state,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DaemonSubsystem {
    Control,
    Backend,
    Frontend,
    Mounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Starting,
    Healthy,
    Degraded,
    Unhealthy,
}

/// Backend serving a daemon. The CLI reads this (and the launch record) instead
/// of inferring the backend from `[system].runtime`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonBackend {
    /// Daemon spawned as a host-native child process.
    Native { pid: u32 },
    /// Daemon running inside a Docker container.
    Docker {
        container_name: String,
        image: String,
    },
}

impl Default for DaemonBackend {
    fn default() -> Self {
        Self::Docker {
            container_name: String::new(),
            image: String::new(),
        }
    }
}

impl DaemonBackend {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Native { .. } => "native",
            Self::Docker { .. } => "container",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FsType {
    Fuse,
    Nfs,
}

impl std::fmt::Display for FsType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fuse => f.write_str("fuse"),
            Self::Nfs => f.write_str("nfs"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FrontendInfo {
    pub source: String,
    pub fs_type: FsType,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MountInfo {
    pub mount: String,
    /// Provider NAME slug. Field name kept as `provider_id` for wire stability; rename deferred to an `API_MAJOR` bump.
    pub provider_id: String,
    pub root_mount: bool,
}

/// One mount that did not converge during a reconcile. `mount` is the mount
/// name, or the spec path when the name could not be parsed.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MountFailure {
    pub mount: String,
    pub kind: ErrorCode,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MountReport {
    pub mount: String,
    pub outcome: MountOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<MountFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<UpgradeDelta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MountOutcome {
    Added,
    Updated,
    Removed,
    Unchanged,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MountUpdateRequest {
    pub spec: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<UpgradeDelta>,
}

/// Wire representation of a reviewed provider-upgrade delta.
///
/// The control API keeps this as transport data, not as a credential-bearing
/// mount spec. The daemon converts it back into the workspace upgrade model and
/// checks that it covers the actual diff computed at the hot-swap boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpgradeDelta {
    Identical,
    AdditiveConfig {
        added: Vec<AddedField>,
    },
    BreakingConfig {
        changes: Vec<FieldChange>,
    },
    CapabilityLimitOrAuth {
        capabilities: Vec<CapabilityChange>,
        limits: Vec<LimitChange>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<AuthDelta>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AddedField {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldChange {
    Added {
        name: String,
        required: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<serde_json::Value>,
    },
    Removed {
        name: String,
    },
    BecameRequired {
        name: String,
    },
    BecameOptional {
        name: String,
    },
    TypeChanged {
        name: String,
        old: serde_json::Value,
        new: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CapabilityChange {
    pub kind: String,
    pub value: String,
    pub direction: CapabilityDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityDirection {
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LimitChange {
    pub name: String,
    pub value: String,
    pub direction: LimitDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitDirection {
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<AuthSurface>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<AuthSurface>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthSurface {
    pub default: String,
    pub schemes: Vec<AuthSchemeSurface>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthSchemeSurface {
    pub key: String,
    pub scheme: serde_json::Value,
}

/// `POST /v1/reconcile`: what converging the running mount set to the on-disk
/// desired state changed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct ReconcileReport {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub updated: Vec<String>,
    pub failed: Vec<MountFailure>,
}

/// Optional request body for `POST /v1/reconcile`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct ReconcileRequest {
    #[serde(default)]
    pub mounts: Vec<String>,
}

/// `POST /v1/shutdown`: what the daemon tore down before exiting.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StopReport {
    pub frontend: Option<FrontendInfo>,
    #[schema(value_type = String)]
    pub mount_point: PathBuf,
    pub providers_dropped: usize,
}
