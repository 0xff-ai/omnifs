//! Shared control-plane domain and wire types for the `omnifs` CLI and daemon.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

pub use omnifs_core::{FrontendRuntime, FsType};

mod control;

pub use control::{
    CONTROL_MAX_LINE_BYTES, CONTROL_PROTOCOL_VERSION, CONTROL_REQUEST_TIMEOUT_SECS, ControlError,
    ControlErrorCode, ControlOperation, ControlOutcome, ControlReply, ControlRequest,
};

/// JSONL activity-event schema and redaction for the inspector observability
/// subsystem.
pub mod events;

/// TCP namespace attach address, injected by a guest frontend launcher and
/// read by `omnifs-thin` when no local `--attach` path is given. Docker uses
/// `host.docker.internal:<port>` to reach the host-native daemon's fixed TCP
/// listener. The listener currently has no authentication.
pub const OMNIFS_ATTACH_ADDR_ENV: &str = "OMNIFS_ATTACH_ADDR";

/// Guest vsock port the frontend runner dials on host CID (`VMADDR_CID_HOST`)
/// once its FUSE mount is serving, writing a single `ready\n` line so the
/// libkrun runner's `launch` can observe guest readiness without an
/// external probe into the guest (the Docker runner instead polls the
/// mount path via `docker exec` from outside the container). Set only by the
/// libkrun runner's seed (`omnifs-seed.conf`); absent on the Docker path.
/// The runner treats this env being set on a non-Linux target as a hard
/// error rather than silently ignoring it, since only the Linux libkrun
/// guest can dial vsock.
pub const OMNIFS_READY_VSOCK_PORT_ENV: &str = "OMNIFS_READY_VSOCK_PORT";

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
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// TCP namespace endpoint this daemon bound for guest frontends.
    pub attach_tcp: Option<SocketAddr>,
    /// Every filesystem frontend currently attached to the shared namespace.
    pub frontends: Vec<FrontendInfo>,
    /// Provider mounts loaded in the registry.
    pub mounts: Vec<MountInfo>,
    /// True when this daemon serves validated durable projections only.
    pub offline: bool,
    /// Daemon-owned health for runtime subsystems. CLI status renders these
    /// entries instead of reconstructing daemon health from raw fields.
    pub health: Box<DaemonHealth>,
}

impl DaemonStatus {
    #[must_use]
    pub fn ready(&self) -> bool {
        self.health.frontend.state == HealthState::Healthy
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonHealth {
    pub control: HealthReport,
    pub frontend: HealthReport,
    pub mounts: HealthReport,
}

impl DaemonHealth {
    #[must_use]
    pub fn new(control: HealthReport, frontend: HealthReport, mounts: HealthReport) -> Self {
        Self {
            control,
            frontend,
            mounts,
        }
    }

    #[must_use]
    pub fn overall_state(&self) -> HealthState {
        let reports = [&self.control, &self.frontend, &self.mounts];
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
pub struct FrontendInfo {
    pub fs_type: FsType,
    /// The frontend-reported mount point. It is host-visible for the host
    /// runner and display-only for Docker and libkrun guests.
    pub mount_point: PathBuf,
    /// How the launcher delivered this frontend. The frontend carries it in
    /// every namespace handshake.
    pub runtime: FrontendRuntime,
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
    use super::{CredentialHealth, FrontendInfo, FrontendRuntime};

    #[test]
    fn frontend_info_round_trips_runtime() {
        let frontend: FrontendInfo = serde_json::from_value(serde_json::json!({
            "fs_type": "nfs",
            "mount_point": "/omnifs",
            "runtime": "host"
        }))
        .unwrap();

        let round_trip: FrontendInfo =
            serde_json::from_value(serde_json::to_value(&frontend).unwrap()).unwrap();
        assert_eq!(round_trip.mount_point, std::path::Path::new("/omnifs"));
        assert_eq!(round_trip.runtime, FrontendRuntime::Host);

        assert!(
            serde_json::from_value::<FrontendInfo>(serde_json::json!({
                "fs_type": "nfs",
                "runtime": "host"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<FrontendInfo>(serde_json::json!({
                "fs_type": "nfs",
                "mount_point": "/omnifs",
                "runtime": "host",
                "unexpected": true
            }))
            .is_err()
        );
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
}
