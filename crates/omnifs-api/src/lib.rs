//! Control API types shared by the `omnifs` CLI and the `omnifsd` daemon.
//!
//! The daemon serves these under `/v1/` on its control listener (TCP
//! loopback through the container port forward today; a Unix socket in the
//! future host-native mode). See `docs/design/daemon-cli-split.md`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use utoipa::ToSchema;

/// Control API version. Bump on breaking changes to routes or payloads;
/// the CLI refuses to manage a daemon with a different major version.
pub const API_VERSION: u32 = 1;

/// Default control port. The container publishes it on the host loopback;
/// both binaries default to it so `omnifs` finds `omnifsd` with zero config.
pub const DEFAULT_PORT: u16 = 7878;

/// `GET /v1/version`
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct VersionInfo {
    pub version: String,
    pub api_version: u32,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    #[schema(value_type = String)]
    pub executable: PathBuf,
}

/// `GET /v1/ready`: 200 with `ready: true` once the filesystem is
/// serving; 503 with `ready: false` while starting up.
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
    #[serde(default)]
    pub api_version: u32,
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
    /// How this daemon was launched, so the CLI tears down and reports the right
    /// backend without inferring it from configuration. A daemon old enough to
    /// omit the field predates host-native and is read as a container.
    #[serde(default)]
    pub launch: LaunchKind,
    /// Provider mounts loaded in the registry.
    pub mounts: Vec<MountInfo>,
    /// Mounts that did not converge at the last reconcile, with reasons. Empty
    /// when every desired mount is serving; a dark mount appears here, not as a
    /// silent absence from `mounts`.
    #[serde(default)]
    pub failed: Vec<MountFailure>,
}

/// How a daemon was launched. The CLI reads this (and the launch record) instead
/// of inferring the backend from `[system].runtime`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LaunchKind {
    /// Daemon spawned as a host-native child process.
    HostNative,
    /// Daemon running inside a Docker container. The legacy interpretation for a
    /// status payload that omits the field.
    #[default]
    Container,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FrontendInfo {
    pub source: String,
    pub fs_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MountInfo {
    pub mount: String,
    pub provider_id: String,
    pub root_mount: bool,
}

/// One mount that did not converge during a reconcile. `mount` is the mount
/// name, or the spec path when the name could not be parsed.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MountFailure {
    pub mount: String,
    pub reason: String,
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

/// `POST /v1/shutdown`: what the daemon tore down before exiting.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StopReport {
    pub frontend: Option<FrontendInfo>,
    #[schema(value_type = String)]
    pub mount_point: PathBuf,
    pub providers_dropped: usize,
}
