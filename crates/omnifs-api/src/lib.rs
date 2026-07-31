//! Shared control-plane domain and wire types for the `omnifs` CLI and daemon.

use omnifs_core::ResourceRevision;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

mod control;
mod credential;
mod filesystem;
mod mount;
mod progress;
mod resource;

/// Protobuf wire types and strict conversions for the local control API.
pub mod grpc;

pub use control::{
    ActionKind, ActionPhase, ActionReceipt, CONTROL_LOG_TAIL_MAX_LINES, CONTROL_MESSAGE_MAX_BYTES,
    CONTROL_REQUEST_TIMEOUT_SECS, CONTROL_RESOURCE_MAX_COUNT, CONTROL_SHUTDOWN_DRAIN_SECS,
    CONTROL_SHUTDOWN_TIMEOUT_SECS, CONTROL_STREAM_ITEM_MAX_BYTES, CONTROL_STREAM_PAYLOAD_MAX_BYTES,
    ControlError, ControlErrorCode, CredentialReceipt, ProviderImportDisposition,
    ProviderImportReceipt, ProviderReference, RevokeCredentialRequest,
    SetCredentialMaterialRequest,
};
pub use credential::{
    CredentialClientOverrides, CredentialKey, CredentialKind, CredentialMaterial, CredentialStatus,
    CredentialStatusKind, CredentialSubmission, SecretBytes,
};
pub use filesystem::{
    FilesystemAccess, FilesystemCommand, FilesystemDefinition, FilesystemPhase, FilesystemStatus,
    GetFilesystemAccessRequest, RestartFilesystemRequest,
};
pub use mount::{MountCredential, MountDefinition, MountHealth, MountLimits, MountRecord};
pub use progress::{
    CredentialProgress, CredentialProgressStage, FilesystemProgress, FilesystemProgressStage,
    ProgressEvent, ProgressEventKind, ProgressSnapshot, ProgressTarget,
    ProviderPreparationProgress, ProviderPreparationStage, ServingProgress, ServingProgressStage,
};
pub use resource::{
    API_VERSION, ApplyReceipt, ApplyResourcesRequest, CredentialDefinition,
    CredentialMaterialSidecar, MountResourceDefinition, NormalizedResourceSet, ProviderDefinition,
    ResourceChange, ResourceChangeAction, ResourceDeclarations, ResourceDefinition,
    ResourceDefinitionError, ResourceLimits, ResourcePhase, ResourcePlan, ResourceSnapshot,
    ResourceStatus, plan,
};

/// JSONL activity-event schema and redaction for the inspector observability
/// subsystem.
pub mod events;

/// TCP namespace attach address, injected by a guest filesystem launcher and
/// read by `omnifs-thin` when no local `--attach` path is given. Docker uses
/// `host.docker.internal:<port>` to reach the host-native daemon's fixed TCP
/// listener. The listener currently has no authentication.
pub const OMNIFS_ATTACH_ADDR_ENV: &str = "OMNIFS_ATTACH_ADDR";

/// Guest vsock port the filesystem runner dials on host CID (`VMADDR_CID_HOST`)
/// once its FUSE mount is serving, writing a single `ready\n` line so the
/// libkrun runner's `launch` can observe guest readiness without an
/// external probe into the guest (the Docker runner instead polls the
/// mount path via `docker exec` from outside the container). Set only by the
/// libkrun runner's seed (`omnifs-seed.conf`); absent on the Docker path.
/// The runner treats this env being set on a non-Linux target as a hard
/// error rather than silently ignoring it, since only the Linux libkrun
/// guest can dial vsock.
pub const OMNIFS_READY_VSOCK_PORT_ENV: &str = "OMNIFS_READY_VSOCK_PORT";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderMetadata {
    pub reference: ProviderReference,
    /// Validated provider manifest document in its native JSON wire format.
    pub manifest: Vec<u8>,
}

/// The daemon's process and namespace attach endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonInfo {
    pub version: String,
    pub pid: u32,
    pub instance_id: String,
    pub executable: PathBuf,
    pub attach_unix: Option<PathBuf>,
    pub attach_tcp: Option<SocketAddr>,
}

/// Opaque identity of one daemon recovery offer.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecoveryId([u8; 16]);

impl RecoveryId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl std::fmt::Debug for RecoveryId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RecoveryId([REDACTED])")
    }
}

/// One explicit repair the recovering daemon can perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairAction {
    RecreateControlStore,
}

/// Repairs currently offered for one exact recovery state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryOffer {
    pub id: RecoveryId,
    pub actions: Vec<RepairAction>,
}

/// Non-path result of recreating the control store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairDisposition {
    FreshStoreCreated,
    CorruptStoreArchived,
}

/// Receipt for one daemon-owned state repair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairReceipt {
    pub instance_id: String,
    pub recovery_id: RecoveryId,
    pub action: RepairAction,
    pub disposition: RepairDisposition,
}

/// Durable and serving state exposed while the daemon recovers its store or
/// namespace. The revisions are independent because reconciliation may fail
/// after the desired resource transaction commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonRecovery {
    pub phase: DaemonPhase,
    pub durable_revision: Option<ResourceRevision>,
    pub serving_revision: Option<ResourceRevision>,
    pub store_health: HealthReport,
    pub repair: Option<RecoveryOffer>,
}

/// Daemon-owned application facts returned without reading daemon state files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonInventory {
    pub info: DaemonInfo,
    pub phase: DaemonPhase,
    pub durable_revision: Option<ResourceRevision>,
    pub serving_revision: Option<ResourceRevision>,
    pub health: DaemonHealth,
    pub mounts: Vec<MountRecord>,
    pub credentials: Vec<CredentialStatus>,
    pub filesystems: Vec<FilesystemDefinition>,
}

/// Current daemon lifecycle phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonPhase {
    Starting,
    Ready,
    RecoveryRequired,
}

/// The daemon's runtime facts, loaded mounts, and non-secret operational health.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    /// Random 16-hex-character id generated per daemon start. The CLI asserts it
    /// against the daemon record it resolved from, so a record overwritten by a
    /// restart mid-command is detected instead of silently trusted.
    pub instance_id: String,
    pub executable: PathBuf,
    /// TCP namespace endpoint this daemon bound for guest filesystems.
    pub attach_tcp: Option<SocketAddr>,
    /// Every configured Filesystem currently attached to the shared namespace.
    pub filesystems: Vec<FilesystemDefinition>,
    /// Provider mounts loaded in the registry.
    pub mounts: Vec<MountInfo>,
    /// Daemon-owned health for runtime subsystems. CLI status renders these
    /// entries instead of reconstructing daemon health from raw fields.
    pub health: Box<DaemonHealth>,
}

impl DaemonStatus {
    #[must_use]
    pub fn ready(&self) -> bool {
        self.health.filesystems.state == HealthState::Healthy
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonHealth {
    pub control: HealthReport,
    pub filesystems: HealthReport,
    pub mounts: HealthReport,
}

impl DaemonHealth {
    #[must_use]
    pub fn new(control: HealthReport, filesystems: HealthReport, mounts: HealthReport) -> Self {
        Self {
            control,
            filesystems,
            mounts,
        }
    }

    #[must_use]
    pub fn overall_state(&self) -> HealthState {
        let reports = [&self.control, &self.filesystems, &self.mounts];
        if reports
            .iter()
            .any(|entry| entry.state == HealthState::Unhealthy)
        {
            HealthState::Unhealthy
        } else if reports
            .iter()
            .any(|entry| entry.state == HealthState::Degraded)
        {
            HealthState::Degraded
        } else if reports
            .iter()
            .any(|entry| entry.state == HealthState::Starting)
        {
            HealthState::Starting
        } else {
            HealthState::Healthy
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthReport {
    pub state: HealthState,
    pub message: String,
}

impl HealthReport {
    #[must_use]
    pub fn new(state: HealthState, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Starting,
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountInfo {
    pub mount: String,
    /// Provider NAME slug, e.g. `github`; credentials key on this value.
    pub provider_name: String,
    /// Pinned provider content hash for the exact WASM artifact this mount runs.
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_health: Option<CredentialHealth>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialHealth {
    Ready,
    ExpiringSoon,
    Expired,
    RefreshFailed,
    NeedsConsent,
    Missing,
    StaticUnvalidated,
}

impl CredentialHealth {
    /// True when the credential needs user action now. `StaticUnvalidated` is
    /// the permanent steady state of a static-token credential (there is no
    /// way to validate it without upstream traffic) and `ExpiringSoon` is the
    /// refresh scheduler's job, so neither degrades status, nudges, or
    /// doctor verdicts.
    #[must_use]
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::Expired | Self::RefreshFailed | Self::NeedsConsent | Self::Missing
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CredentialHealth, DaemonInfo, DaemonPhase, DaemonRecovery, HealthReport, HealthState,
        RecoveryId, RecoveryOffer, RepairAction, RepairDisposition, RepairReceipt,
    };
    use omnifs_core::ResourceRevision;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::PathBuf;

    #[test]
    fn steady_state_healths_do_not_need_attention() {
        assert!(!CredentialHealth::Ready.needs_attention());
        assert!(!CredentialHealth::StaticUnvalidated.needs_attention());
        assert!(!CredentialHealth::ExpiringSoon.needs_attention());
        assert!(CredentialHealth::Expired.needs_attention());
        assert!(CredentialHealth::RefreshFailed.needs_attention());
        assert!(CredentialHealth::NeedsConsent.needs_attention());
        assert!(CredentialHealth::Missing.needs_attention());
    }

    #[test]
    fn daemon_info_and_recovery_round_trip_without_private_paths() {
        let info = DaemonInfo {
            version: "0.1.0".to_owned(),
            pid: 42,
            instance_id: "0123456789abcdef".to_owned(),
            executable: PathBuf::from("/usr/local/bin/omnifs"),
            attach_unix: Some(PathBuf::from("/tmp/omnifs-local.sock")),
            attach_tcp: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9000)),
        };
        let info_json = serde_json::to_value(&info).unwrap();
        assert!(info_json.get("config_dir").is_none());
        assert!(info_json.get("cache_dir").is_none());
        assert!(info_json.get("store").is_none());

        let recovery = DaemonRecovery {
            phase: DaemonPhase::RecoveryRequired,
            durable_revision: Some(ResourceRevision::new(7)),
            serving_revision: None,
            store_health: HealthReport::new(HealthState::Degraded, "store unavailable"),
            repair: Some(RecoveryOffer {
                id: RecoveryId::from_bytes([0x22; 16]),
                actions: vec![RepairAction::RecreateControlStore],
            }),
        };
        let encoded = serde_json::to_vec(&recovery).unwrap();
        assert_eq!(
            serde_json::from_slice::<DaemonRecovery>(&encoded).unwrap(),
            recovery
        );
        assert_eq!(
            serde_json::to_value(DaemonPhase::RecoveryRequired).unwrap(),
            "recovery_required"
        );
    }

    #[test]
    fn recovery_ids_are_exact_opaque_tokens() {
        let current = RecoveryId::from_bytes([0xab; 16]);
        let stale = RecoveryId::from_bytes([0xcd; 16]);
        assert_eq!(current.as_bytes(), &[0xab; 16]);
        assert_ne!(current, stale);
        assert_eq!(format!("{current:?}"), "RecoveryId([REDACTED])");

        let encoded = serde_json::to_vec(&current).unwrap();
        assert_eq!(
            serde_json::from_slice::<RecoveryId>(&encoded).unwrap(),
            current
        );
        assert!(serde_json::from_value::<RecoveryId>(serde_json::json!(vec![0_u8; 15])).is_err());
        assert!(serde_json::from_value::<RecoveryId>(serde_json::json!(vec![0_u8; 17])).is_err());
    }

    #[test]
    fn recovery_offer_and_receipt_have_no_path_fields() {
        let offer = RecoveryOffer {
            id: RecoveryId::from_bytes([1; 16]),
            actions: vec![RepairAction::RecreateControlStore],
        };
        let receipt = RepairReceipt {
            instance_id: "instance".to_owned(),
            recovery_id: offer.id,
            action: RepairAction::RecreateControlStore,
            disposition: RepairDisposition::CorruptStoreArchived,
        };

        let offer_json = serde_json::to_value(&offer).unwrap();
        let offer_fields = offer_json.as_object().unwrap();
        assert_eq!(offer_fields.len(), 2);
        assert!(offer_fields.contains_key("id"));
        assert!(offer_fields.contains_key("actions"));

        let receipt_json = serde_json::to_value(&receipt).unwrap();
        let receipt_fields = receipt_json.as_object().unwrap();
        assert_eq!(receipt_fields.len(), 4);
        assert!(receipt_fields.contains_key("instance_id"));
        assert!(receipt_fields.contains_key("recovery_id"));
        assert!(receipt_fields.contains_key("action"));
        assert!(receipt_fields.contains_key("disposition"));
        assert_eq!(
            serde_json::to_value(RepairAction::RecreateControlStore).unwrap(),
            "recreate_control_store"
        );
        assert_eq!(
            serde_json::to_value(RepairDisposition::CorruptStoreArchived).unwrap(),
            "corrupt_store_archived"
        );
    }
}
