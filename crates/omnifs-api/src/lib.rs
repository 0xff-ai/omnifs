//! Control API types shared by the `omnifs` CLI and daemon runtime.
//!
//! The daemon serves these under `/v1/` on its control listener: a Unix
//! domain socket for the host-native daemon. See
//! `docs/contracts/50-control-plane.md`.

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
pub const API_MAJOR: u16 = 8;

/// Control API minor version. The CLI warns but proceeds when the daemon's
/// minor differs. Bump for additive, backward-compatible additions.
pub const API_MINOR: u16 = 0;

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
    MountNotFound,
    SpecInvalid,
    DaemonShuttingDown,
    Internal,
}

/// `GET /v1/ready`: 200 with `ready: true` once the immutable mount revision loads
/// and every requested namespace listener is serving.
/// Non-ready responses use [`ApiError`].
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReadyInfo {
    pub ready: bool,
}

/// `GET /v1/status`: the daemon's runtime facts, loaded mounts, and non-secret
/// operational health. Credentials are represented only by coarse health.
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
    /// Random 16-hex-character id generated per daemon start. The CLI asserts it
    /// against the runtime record it resolved from, so a record overwritten by a
    /// restart mid-command is detected instead of silently trusted.
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    #[schema(value_type = String)]
    pub executable: PathBuf,
    #[schema(value_type = String)]
    pub config_dir: PathBuf,
    #[schema(value_type = String)]
    pub cache_dir: PathBuf,
    #[schema(value_type = String)]
    pub providers_dir: PathBuf,
    /// Every filesystem frontend currently attached to the shared namespace.
    #[serde(default)]
    pub frontends: Vec<FrontendInfo>,
    /// Backend serving this daemon, so the CLI tears down and reports the right
    /// backend without inferring it from configuration. Missing identity is not
    /// reclaimable; teardown stops instead of guessing.
    #[serde(default, alias = "launch")]
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

/// Backend serving a daemon. The daemon always runs host-native; the CLI reads
/// this (and the runtime record) rather than assuming it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
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
    /// The frontend-reported mount point. It is host-visible for the local
    /// driver and display-only for Docker and krunkit guests.
    #[serde(default)]
    #[schema(value_type = String)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum FrontendDelivery {
    /// Attached over the fixed `frontends/local.sock` Unix domain socket.
    Local,
    /// Attached over the TCP namespace listener (`POST
    /// /v1/frontend/attach-target`), the Docker Desktop delivery path. The
    /// default: `FrontendAttachTargetRequest.driver` defaults to it, and it
    /// is the only value that route accepts today.
    Docker,
    /// Attached over the token-checking UDS vsock-proxy listener (`POST
    /// /v1/frontend/attach-target/vsock`), the krunkit-on-macOS delivery path.
    Krunkit,
}

impl FrontendDelivery {
    const fn default_local() -> Self {
        Self::Local
    }

    fn default_attach() -> Self {
        Self::Docker
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MountInfo {
    pub mount: String,
    /// Provider NAME slug, e.g. `github`; credentials key on this value.
    pub provider_name: String,
    /// Pinned provider content hash for the exact WASM artifact this mount runs.
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_health: Option<CredentialHealth>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProviderArtifact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub id_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProviderSummary {
    pub name: String,
    pub installed: Vec<ProviderArtifact>,
}

/// `POST /v1/shutdown`: daemon state immediately before exit.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StopReport {
    /// Every attached frontend at the moment of shutdown.
    pub frontends: Vec<FrontendInfo>,
    pub providers_dropped: usize,
}

/// Optional request body for `POST /v1/frontend/attach-target`. The address is
/// honored only on the first bind (the listener is idempotent thereafter).
/// An absent `bind_ip` selects loopback; native Linux may request the default
/// Docker bridge gateway, which the daemon validates before binding.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FrontendAttachTargetRequest {
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub bind_ip: Option<std::net::Ipv4Addr>,
    /// The delivery mechanism attaching over this listener. Only `docker` is
    /// accepted today (krunkit attaches over vsock instead, through the
    /// separate `/v1/frontend/attach-target/vsock` route); any other value is
    /// a 400. Defaults to `docker` when omitted.
    #[serde(default = "FrontendDelivery::default_attach")]
    pub driver: FrontendDelivery,
}

impl Default for FrontendAttachTargetRequest {
    fn default() -> Self {
        Self {
            bind_ip: None,
            driver: FrontendDelivery::default_attach(),
        }
    }
}

/// `POST /v1/frontend/attach-target`: the TCP attach target a frontend dials
/// (address plus per-instance attach token), whether just bound or already
/// serving from an earlier call (or from `--attach-tcp` at daemon start).
/// Named after what it returns: the Omnifs VFS wire protocol client's
/// `AttachTarget::Tcp`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FrontendAttachTargetReport {
    pub addr: String,
    pub token: String,
}

/// `POST /v1/frontend/attach-target/vsock`: the vsock attach target's host
/// side, a token-checking UDS listener's socket path plus the per-instance
/// attach token the guest presents (the Omnifs VFS wire protocol client's
/// `AttachTarget::Vsock`).
/// This is the krunkit-on-macOS path: a guest VM has no shared host Unix
/// socket and no Docker-style loopback either, so it dials host vsock instead,
/// and krunkit proxies every vsock connection onto `socket_path`. Every
/// connection krunkit forwards looks like the same trusted local peer to that
/// socket, so `token` (not filesystem permissions) is this listener's real
/// auth, checked the same way the TCP listener's is. Takes no request body:
/// unlike the TCP listener, there is no bind address to choose, only the
/// daemon-picked path under the workspace. Idempotent, same as
/// [`FrontendAttachTargetReport`]: a repeat call returns the already-bound
/// path and token unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FrontendAttachTargetVsockReport {
    pub socket_path: String,
    pub token: String,
}

#[cfg(test)]
mod tests {
    use super::{CredentialHealth, FrontendAttachTargetRequest, FrontendDelivery, FrontendInfo};

    #[test]
    fn frontend_attach_request_defaults_to_docker() {
        let request: FrontendAttachTargetRequest =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(request.driver, FrontendDelivery::Docker);
    }

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
