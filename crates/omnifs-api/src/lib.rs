//! Shared control-plane domain and wire types for the `omnifs` CLI and daemon.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod control;

pub use control::{
    CONTROL_MAX_LINE_BYTES, CONTROL_PROTOCOL_VERSION, CONTROL_REQUEST_TIMEOUT_SECS, ControlError,
    ControlErrorCode, ControlOperation, ControlOutcome, ControlReply, ControlRequest,
    TcpAttachTarget, VsockAttachTarget,
};

/// JSONL activity-event schema and redaction for the inspector observability
/// subsystem.
pub mod events;

/// TCP namespace attach address, injected by the frontend container launcher
/// and read by the out-of-process `omnifs-thin fuse` runner when no `--attach`
/// unix path is given. Carries `host.docker.internal:<port>` so a
/// containerized frontend reaches the host-native daemon's TCP attach
/// listener.
pub const OMNIFS_ATTACH_ADDR_ENV: &str = "OMNIFS_ATTACH_ADDR";

/// The per-instance attach token paired with [`OMNIFS_ATTACH_ADDR_ENV`],
/// authenticating the TCP namespace attach handshake in place of filesystem
/// permissions.
pub const OMNIFS_ATTACH_TOKEN_ENV: &str = "OMNIFS_ATTACH_TOKEN";

/// Guest vsock port the frontend runner dials on host CID (`VMADDR_CID_HOST`)
/// once its FUSE mount is serving, writing a single `ready\n` line so the
/// krunkit backend's `launch` can observe guest readiness without an
/// external probe into the guest (the Docker backend instead polls the
/// mount path via `docker exec` from outside the container). Set only by the
/// krunkit backend's seed (`omnifs-seed.conf`); absent on the Docker path.
/// The runner treats this env being set on a non-Linux target as a hard
/// error rather than silently ignoring it, since only the Linux krunkit
/// guest can dial vsock.
pub const OMNIFS_READY_VSOCK_PORT_ENV: &str = "OMNIFS_READY_VSOCK_PORT";

/// The daemon's runtime facts, loaded mounts, and non-secret operational health.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    #[serde(default)]
    pub pid: u32,
    /// Random 16-hex-character id generated per daemon start. The CLI asserts it
    /// against the runtime record it resolved from, so a record overwritten by a
    /// restart mid-command is detected instead of silently trusted.
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    pub executable: PathBuf,
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub providers_dir: PathBuf,
    /// Every filesystem frontend currently attached to the shared namespace.
    #[serde(default)]
    pub frontends: Vec<FrontendInfo>,
    /// Backend serving this daemon, so the CLI tears down and reports the right
    /// backend without inferring it from configuration. Missing identity is not
    /// reclaimable; teardown stops instead of guessing.
    #[serde(default)]
    pub backend: DaemonBackend,
    /// Provider mounts loaded in the registry.
    pub mounts: Vec<MountInfo>,
    /// Daemon-owned health for runtime subsystems. CLI status renders these
    /// entries instead of reconstructing daemon health from raw fields.
    #[serde(default)]
    pub health: DaemonHealth,
}

impl DaemonStatus {
    #[must_use]
    pub fn ready(&self) -> bool {
        self.health
            .subsystem(DaemonSubsystem::Frontend)
            .map_or(!self.frontends.is_empty(), |entry| {
                entry.state == HealthState::Healthy
            })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonSubsystem {
    Control,
    Backend,
    Frontend,
    Mounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Starting,
    Healthy,
    Degraded,
    Unhealthy,
}

/// Backend serving a daemon. The daemon always runs host-native; the CLI reads
/// this (and the runtime record) rather than assuming it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonBackend {
    /// Daemon spawned as a host-native child process.
    Native { pid: u32 },
}

impl Default for DaemonBackend {
    fn default() -> Self {
        Self::Native { pid: 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendInfo {
    pub source: String,
    pub fs_type: FsType,
    /// The frontend-reported mount point. It is host-visible for the local
    /// driver and display-only for Docker and krunkit guests.
    #[serde(default)]
    pub mount_point: PathBuf,
    /// How this frontend reaches the shared namespace. The host assigns this
    /// from which listener the connection arrived on, never from anything a
    /// connecting guest claims about itself.
    #[serde(default = "FrontendDelivery::default_local")]
    pub delivery: FrontendDelivery,
}

/// How a frontend is delivered to the shared namespace. Assigned by the host
/// at bind time per listener, never
/// self-reported by the connecting guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrontendDelivery {
    /// Attached over the fixed `frontends/local.sock` Unix domain socket.
    Local,
    /// Attached over the TCP namespace listener, the Docker Desktop delivery
    /// path.
    Docker,
    /// Attached over the token-checking UDS vsock-proxy listener, the
    /// krunkit-on-macOS delivery path.
    Krunkit,
}

impl FrontendDelivery {
    const fn default_local() -> Self {
        Self::Local
    }
}

impl std::fmt::Display for FrontendDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Local => "local",
            Self::Docker => "docker",
            Self::Krunkit => "krunkit",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    use super::{CredentialHealth, FrontendDelivery, FrontendInfo};

    #[test]
    fn legacy_frontend_info_defaults_to_local_delivery() {
        let frontend: FrontendInfo = serde_json::from_value(serde_json::json!({
            "source": "native",
            "fs_type": "nfs"
        }))
        .unwrap();

        assert!(frontend.mount_point.as_os_str().is_empty());
        assert_eq!(frontend.delivery, FrontendDelivery::Local);
    }

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
    fn credential_wire_types_do_not_reference_secret_types() {
        let source = include_str!("lib.rs");

        assert!(!source.contains(concat!("Header", "Material")));
        assert!(!source.contains(concat!("Secret", "String")));
        assert!(!source.contains(concat!("omnifs", "_auth")));
    }
}
