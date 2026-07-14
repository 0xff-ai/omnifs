//! Authoritative application inventory.
//!
//! This module owns the typed facts consumed by status, list, and receipt
//! surfaces. Collection is deliberately at the edge; all joins, sorting, and
//! verdict decisions below are pure.

use anyhow::Result;
use omnifs_api::{DaemonStatus, FrontendDelivery, FsType, HealthState};
use omnifs_mtab::{MountKind, MountState};
use omnifs_workspace::config::{Config, EffectiveFrontend, Environment, Filesystem, HostOs};
use omnifs_workspace::creds::FileStore;
use omnifs_workspace::layout::{WorkspaceLayout, resolve_mount_point};
use omnifs_workspace::mounts::{Name as MountName, Registry};
use omnifs_workspace::provider::Catalog;
use omnifs_workspace::runtime_record::RuntimeRecord;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::auth::{AuthReadiness, MountAuth};
use crate::workspace::Workspace;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct Inventory {
    pub(crate) workspace: WorkspaceStatus,
    pub(crate) frontends: Vec<FrontendStatus>,
    pub(crate) mounts: Vec<MountStatus>,
    pub(crate) providers: Vec<ProviderStatus>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct WorkspaceStatus {
    pub(crate) home: PathBuf,
    pub(crate) daemon: DaemonState,
    pub(crate) namespace: NamespaceState,
    pub(crate) pid: Option<u32>,
    pub(crate) api: Option<ApiVersion>,
    pub(crate) runtime_expected: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ApiVersion {
    pub(crate) major: u16,
    pub(crate) minor: u16,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DaemonState {
    Running,
    Starting,
    Degraded,
    Stopped,
    Failed,
    Unreachable,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NamespaceState {
    Serving,
    Offline,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    Positive,
    Neutral,
    Attention,
    Error,
}

impl Severity {
    pub(crate) const fn rank(self) -> u8 {
        match self {
            Self::Positive => 0,
            Self::Neutral => 1,
            Self::Attention => 2,
            Self::Error => 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct FrontendStatus {
    pub(crate) filesystem: Filesystem,
    pub(crate) environment: Environment,
    pub(crate) location: Option<PathBuf>,
    pub(crate) source: FrontendSource,
    pub(crate) state: FrontendState,
    pub(crate) scope: &'static str,
    pub(crate) mount_count: usize,
    pub(crate) fix: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FrontendSource {
    PlatformDefault,
    Configured,
    Unmanaged,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FrontendState {
    Attached,
    Stopped,
    Unattached,
    Unmanaged,
    Failed,
}

impl FrontendState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Stopped => "stopped",
            Self::Unattached => "unattached",
            Self::Unmanaged => "unmanaged",
            Self::Failed => "failed",
        }
    }

    pub(crate) const fn severity(self) -> Severity {
        match self {
            Self::Attached => Severity::Positive,
            Self::Stopped => Severity::Neutral,
            Self::Unattached | Self::Unmanaged => Severity::Attention,
            Self::Failed => Severity::Error,
        }
    }

    pub(crate) const fn fix(self) -> Option<&'static str> {
        match self {
            Self::Unattached => Some("omnifs up"),
            Self::Unmanaged => Some("omnifs frontend disable"),
            Self::Failed => Some("omnifs logs"),
            Self::Attached | Self::Stopped => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct MountStatus {
    pub(crate) name: String,
    pub(crate) root: PathBuf,
    pub(crate) provider: ProviderPin,
    pub(crate) auth: AuthState,
    pub(crate) serving: ServingState,
    pub(crate) access_count: usize,
    pub(crate) fix: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProviderPin {
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    pub(crate) artifact: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum AuthState {
    NotNeeded,
    Ready,
    Missing { command: String },
    Expired { command: String },
    Error { message: String, command: String },
}

impl AuthState {
    pub(crate) fn from_readiness(readiness: &AuthReadiness, mount: &str) -> Self {
        let command = format!("omnifs mount reauth {mount}");
        match readiness {
            AuthReadiness::None => Self::NotNeeded,
            AuthReadiness::Missing { .. } => Self::Missing { command },
            AuthReadiness::Error { message } => Self::Error {
                message: message.clone(),
                command,
            },
            AuthReadiness::Ready { notices, .. }
                if notices.iter().any(|notice| notice.starts_with("expired")) =>
            {
                Self::Expired { command }
            },
            AuthReadiness::Ready { .. } => Self::Ready,
        }
    }

    pub(crate) const fn severity(&self) -> Severity {
        match self {
            Self::NotNeeded => Severity::Neutral,
            Self::Ready => Severity::Positive,
            Self::Missing { .. } | Self::Expired { .. } => Severity::Attention,
            Self::Error { .. } => Severity::Error,
        }
    }

    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::NotNeeded => "not needed",
            Self::Ready => "ready",
            Self::Missing { .. } => "missing",
            Self::Expired { .. } => "expired",
            Self::Error { .. } => "error",
        }
    }

    pub(crate) fn command(&self) -> Option<&str> {
        match self {
            Self::Missing { command } | Self::Expired { command } | Self::Error { command, .. } => {
                Some(command)
            },
            Self::NotNeeded | Self::Ready => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ServingState {
    Live,
    Offline,
    Failed { message: String },
    NotLoaded,
}

impl ServingState {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Offline => "offline",
            Self::Failed { .. } => "failed",
            Self::NotLoaded => "not loaded",
        }
    }

    pub(crate) const fn severity(&self) -> Severity {
        match self {
            Self::Live => Severity::Positive,
            Self::Offline => Severity::Neutral,
            Self::Failed { .. } | Self::NotLoaded => Severity::Error,
        }
    }

    pub(crate) const fn fix(&self) -> Option<&'static str> {
        match self {
            Self::Failed { .. } => Some("omnifs logs"),
            Self::Live | Self::Offline | Self::NotLoaded => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProviderStatus {
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    pub(crate) artifact: String,
    pub(crate) pinned_by: Vec<String>,
    pub(crate) state: ProviderState,
    pub(crate) fix: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderState {
    Pinned,
    Installed,
    Missing,
}

impl ProviderState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::Installed => "installed",
            Self::Missing => "missing",
        }
    }

    pub(crate) const fn severity(self) -> Severity {
        match self {
            Self::Pinned => Severity::Positive,
            Self::Installed => Severity::Neutral,
            Self::Missing => Severity::Attention,
        }
    }

    pub(crate) const fn fix(self) -> Option<&'static str> {
        match self {
            Self::Missing => Some("omnifs provider add <path>"),
            Self::Pinned | Self::Installed => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct AccessPath {
    pub(crate) filesystem: Filesystem,
    pub(crate) environment: Environment,
    pub(crate) path: PathBuf,
    pub(crate) state: AccessState,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccessState {
    Available,
    FrontendStopped,
    Offline,
    Failed,
}

impl AccessState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::FrontendStopped => "frontend stopped",
            Self::Offline => "offline",
            Self::Failed => "failed",
        }
    }
}

impl Inventory {
    pub(crate) async fn collect(workspace: &Workspace) -> Result<Self> {
        let layout = workspace.layout().clone();
        let config = Config::load(&layout.config_file)?;
        let host_os = current_host_os();
        let default_location =
            resolve_mount_point().unwrap_or_else(|| layout.config_dir.join("omnifs"));
        let effective = config.frontends.effective(host_os, &default_location)?;
        let registry = Registry::load(&layout.mounts_dir)?;
        let catalog = workspace.catalog();
        let credentials = FileStore::new(&layout.credentials_file);
        let daemon = workspace.daemon().compatible_status_optional().await;
        let daemon_status = daemon.as_ref().ok().and_then(Option::as_ref);
        let runtime = RuntimeRecord::read(&layout.runtime_record_file())
            .ok()
            .flatten();
        let mut mounts = mount_statuses(&registry, catalog, &credentials, daemon_status);
        let mount_count = mounts.len();
        let local_fallback = local_frontend_fallback(&layout, &effective, mount_count)?;
        let frontends = frontend_statuses(&effective, daemon_status, mount_count, &local_fallback);
        let access_count = frontends
            .iter()
            .filter(|frontend| {
                matches!(
                    frontend.state,
                    FrontendState::Attached | FrontendState::Unmanaged
                )
            })
            .count();
        for mount in &mut mounts {
            mount.access_count = access_count;
        }
        let providers = provider_statuses(&registry, catalog)?;
        let workspace_status = workspace_status(&layout, daemon_status, runtime.as_ref(), &daemon);
        Ok(Self {
            workspace: workspace_status,
            frontends,
            mounts,
            providers,
        })
    }

    pub(crate) fn access_paths(&self, mount: &MountName) -> Vec<AccessPath> {
        let Some(mount_status) = self
            .mounts
            .iter()
            .find(|entry| entry.name == mount.to_string())
        else {
            return Vec::new();
        };
        self.frontends
            .iter()
            .filter_map(|frontend| {
                let location = frontend.location.as_ref()?;
                let path = location.join(
                    mount_status
                        .root
                        .strip_prefix("/")
                        .unwrap_or(&mount_status.root),
                );
                let state = match frontend.state {
                    FrontendState::Attached => match mount_status.serving {
                        ServingState::Live => AccessState::Available,
                        ServingState::Failed { .. } => AccessState::Failed,
                        ServingState::Offline | ServingState::NotLoaded => AccessState::Offline,
                    },
                    FrontendState::Stopped | FrontendState::Unattached => {
                        AccessState::FrontendStopped
                    },
                    FrontendState::Unmanaged => match mount_status.serving {
                        ServingState::Live => AccessState::Available,
                        ServingState::Failed { .. } => AccessState::Failed,
                        ServingState::Offline | ServingState::NotLoaded => AccessState::Offline,
                    },
                    FrontendState::Failed => AccessState::Failed,
                };
                Some(AccessPath {
                    filesystem: frontend.filesystem,
                    environment: frontend.environment,
                    path,
                    state,
                })
            })
            .collect()
    }

    pub(crate) fn verdict(&self) -> Verdict {
        let degraded = self.frontends.iter().any(|entry| {
            entry.state.severity() >= Severity::Attention
                && matches!(
                    self.workspace.daemon,
                    DaemonState::Running | DaemonState::Starting | DaemonState::Degraded
                )
        }) || self.mounts.iter().any(|entry| {
            entry.fix.is_some()
                || entry.auth.severity() >= Severity::Attention
                || entry.serving.severity() >= Severity::Attention
        }) || self
            .providers
            .iter()
            .any(|entry| entry.state == ProviderState::Missing)
            || matches!(
                self.workspace.daemon,
                DaemonState::Failed | DaemonState::Unreachable
            );
        if degraded {
            Verdict::Degraded
        } else {
            Verdict::Ok
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Verdict {
    Ok,
    Degraded,
}

fn current_host_os() -> HostOs {
    if cfg!(target_os = "linux") {
        HostOs::Linux
    } else if cfg!(target_os = "macos") {
        HostOs::MacOs
    } else {
        HostOs::Other
    }
}

fn workspace_status(
    layout: &WorkspaceLayout,
    daemon: Option<&DaemonStatus>,
    runtime: Option<&RuntimeRecord>,
    probe: &Result<Option<DaemonStatus>, anyhow::Error>,
) -> WorkspaceStatus {
    let runtime_expected = runtime.is_some();
    let (daemon_state, namespace, pid, api) = match daemon {
        Some(status) => (
            match status.health.overall_state() {
                HealthState::Healthy => DaemonState::Running,
                HealthState::Starting => DaemonState::Starting,
                HealthState::Degraded => DaemonState::Degraded,
                HealthState::Unhealthy => DaemonState::Failed,
            },
            if status.health.overall_state() == HealthState::Unhealthy {
                NamespaceState::Failed
            } else {
                NamespaceState::Serving
            },
            Some(status.pid),
            Some(ApiVersion {
                major: status.api_major,
                minor: status.api_minor,
            }),
        ),
        None if probe.is_err() && runtime_expected => (
            DaemonState::Unreachable,
            NamespaceState::Offline,
            runtime.map(|record| match record.backend {
                omnifs_workspace::runtime_record::RecordedBackend::Native { pid } => pid,
            }),
            None,
        ),
        None => (DaemonState::Stopped, NamespaceState::Offline, None, None),
    };
    WorkspaceStatus {
        home: layout.config_dir.clone(),
        daemon: daemon_state,
        namespace,
        pid,
        api,
        runtime_expected,
    }
}

/// Discover host-owned runner records when the daemon cannot answer. These
/// records are the runner's observable access surface, so a daemon-down status
/// must retain them instead of reporting every local frontend as stopped.
fn local_frontend_fallback(
    layout: &WorkspaceLayout,
    effective: &[EffectiveFrontend],
    mount_count: usize,
) -> Result<Vec<FrontendStatus>> {
    let mut rows = Vec::new();
    for path in MountState::files_under(&layout.frontend_state_root())? {
        match MountState::read_file(&path) {
            Ok(state) => {
                let filesystem = match state.kind {
                    MountKind::Fuse => Filesystem::Fuse,
                    MountKind::Nfs { .. } => Filesystem::Nfs,
                };
                let configured = effective.iter().any(|entry| {
                    entry.environment == Environment::Host
                        && entry.filesystem == filesystem
                        && entry.location.as_ref() == Some(&state.mount_point)
                });
                let frontend_state = if configured {
                    FrontendState::Attached
                } else {
                    FrontendState::Unmanaged
                };
                rows.push(FrontendStatus {
                    filesystem,
                    environment: Environment::Host,
                    location: Some(state.mount_point),
                    source: if configured {
                        FrontendSource::Configured
                    } else {
                        FrontendSource::Unmanaged
                    },
                    state: frontend_state,
                    scope: "all",
                    mount_count,
                    fix: frontend_state.fix().map(str::to_owned),
                });
            },
            Err(error) => {
                // Keep a corrupt leaf visible as its own degraded row. A bad
                // record must not hide healthy sibling leaves.
                let filesystem = path
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .and_then(|name| match name {
                        "fuse" => Some(Filesystem::Fuse),
                        "nfs" => Some(Filesystem::Nfs),
                        _ => None,
                    })
                    .unwrap_or(Filesystem::Fuse);
                rows.push(FrontendStatus {
                    filesystem,
                    environment: Environment::Host,
                    location: None,
                    source: FrontendSource::Unmanaged,
                    state: FrontendState::Failed,
                    scope: "all",
                    mount_count,
                    fix: Some(format!("omnifs logs ({error})")),
                });
            },
        }
    }
    Ok(rows)
}

fn frontend_statuses(
    effective: &[EffectiveFrontend],
    daemon: Option<&DaemonStatus>,
    mount_count: usize,
    local_fallback: &[FrontendStatus],
) -> Vec<FrontendStatus> {
    let mut rows = effective
        .iter()
        .map(|entry| {
            let attached = daemon.and_then(|status| {
                status.frontends.iter().find(|observed| {
                    observed.fs_type == fs_type(entry.filesystem)
                        && observed.delivery == delivery(entry.environment)
                        && (entry.location.is_none()
                            || Some(&observed.mount_point) == entry.location.as_ref())
                })
            });
            let fallback_attached = local_fallback.iter().any(|candidate| {
                candidate.filesystem == entry.filesystem
                    && candidate.environment == Environment::Host
                    && entry.environment == Environment::Host
                    && entry.location.as_ref() == candidate.location.as_ref()
                    && candidate.state == FrontendState::Attached
            });
            let state = if daemon
                .is_some_and(|status| status.health.overall_state() == HealthState::Unhealthy)
            {
                FrontendState::Failed
            } else if attached.is_some() || fallback_attached {
                FrontendState::Attached
            } else if daemon.is_some() {
                FrontendState::Unattached
            } else {
                FrontendState::Stopped
            };
            FrontendStatus {
                filesystem: entry.filesystem,
                environment: entry.environment,
                location: entry
                    .location
                    .clone()
                    .or_else(|| guest_location(entry.environment).map(PathBuf::from)),
                source: match entry.source {
                    omnifs_workspace::config::PlanSource::PlatformDefault => {
                        FrontendSource::PlatformDefault
                    },
                    omnifs_workspace::config::PlanSource::Configured => FrontendSource::Configured,
                },
                state,
                scope: "all",
                mount_count,
                fix: state.fix().map(str::to_owned),
            }
        })
        .collect::<Vec<_>>();
    if let Some(status) = daemon {
        for observed in &status.frontends {
            let environment = environment(observed.delivery);
            let filesystem = filesystem(observed.fs_type);
            let matched = effective.iter().any(|entry| {
                entry.filesystem == filesystem
                    && entry.environment == environment
                    && (entry.location.is_none()
                        || entry.location.as_ref() == Some(&observed.mount_point))
            });
            if !matched {
                rows.push(FrontendStatus {
                    filesystem,
                    environment,
                    location: Some(observed.mount_point.clone()),
                    source: FrontendSource::Unmanaged,
                    state: FrontendState::Unmanaged,
                    scope: "all",
                    mount_count,
                    fix: FrontendState::Unmanaged.fix().map(str::to_owned),
                });
            }
        }
    }
    if daemon.is_none() {
        let unmatched = local_fallback
            .iter()
            .filter(|candidate| {
                !rows.iter().any(|row| {
                    row.filesystem == candidate.filesystem
                        && row.environment == candidate.environment
                        && row.location == candidate.location
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        rows.extend(unmatched);
    }
    rows.sort_by(frontend_cmp);
    rows
}

fn mount_statuses(
    registry: &Registry,
    catalog: &Catalog,
    credentials: &FileStore,
    daemon: Option<&DaemonStatus>,
) -> Vec<MountStatus> {
    let desired = registry
        .iter()
        .map(|(name, _)| name.to_string())
        .collect::<BTreeSet<_>>();
    let loaded = daemon
        .map(|status| {
            status
                .mounts
                .iter()
                .map(|mount| mount.mount.as_str())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut rows = desired_mount_rows(registry, catalog, credentials, daemon, &loaded);
    rows.extend(invalid_mount_rows(registry));
    if let Some(status) = daemon {
        rows.extend(observed_mount_rows(status, &desired));
    }
    rows.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| left.name.cmp(&right.name))
    });
    rows
}

fn desired_mount_rows(
    registry: &Registry,
    catalog: &Catalog,
    credentials: &FileStore,
    daemon: Option<&DaemonStatus>,
    loaded: &BTreeSet<&str>,
) -> Vec<MountStatus> {
    let daemon_failed =
        daemon.is_some_and(|status| status.health.overall_state() == HealthState::Unhealthy);
    registry
        .iter()
        .map(|(name, spec)| {
            let name_string = name.to_string();
            let artifact = spec.provider.id.to_string();
            let provider = ProviderPin {
                name: spec.provider.meta.name.to_string(),
                version: spec.provider.meta.version.as_ref().map(ToString::to_string),
                artifact,
            };
            let auth = AuthState::from_readiness(
                &MountAuth::from_spec(catalog, spec.clone()).readiness(credentials),
                &name_string,
            );
            let provider_present = catalog.get(&spec.provider.id).ok().flatten().is_some();
            let serving = derive_serving_state(MountObservation {
                provider: if provider_present {
                    Presence::Present
                } else {
                    Presence::Absent
                },
                daemon: if daemon.is_some() {
                    Presence::Present
                } else {
                    Presence::Absent
                },
                loaded: if loaded.contains(name_string.as_str()) {
                    Presence::Present
                } else {
                    Presence::Absent
                },
                health: if daemon_failed {
                    Health::Unhealthy
                } else {
                    Health::Healthy
                },
            });
            // Fixes follow the same precedence as the observed facts: an
            // unreadable spec is emitted below, then artifact retention, auth,
            // daemon failure, and finally the absence of a loaded mount.
            let fix = if !provider_present {
                ProviderState::Missing.fix().map(str::to_owned)
            } else if let Some(command) = auth.command() {
                Some(command.to_owned())
            } else {
                serving.fix().map(str::to_owned)
            };
            MountStatus {
                name: name_string,
                root: PathBuf::from(format!("/{name}")),
                provider,
                auth,
                serving,
                access_count: 0,
                fix,
            }
        })
        .collect::<Vec<_>>()
}

fn invalid_mount_rows(registry: &Registry) -> Vec<MountStatus> {
    registry
        .failures()
        .iter()
        .map(|failure| MountStatus {
            name: failure
                .path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("<invalid>")
                .to_string(),
            root: PathBuf::from("/"),
            provider: ProviderPin {
                name: "<invalid>".into(),
                version: None,
                artifact: String::new(),
            },
            auth: AuthState::Error {
                message: failure.error.to_string(),
                command: "omnifs doctor".into(),
            },
            serving: ServingState::Failed {
                message: failure.error.to_string(),
            },
            access_count: 0,
            fix: Some("omnifs doctor".into()),
        })
        .collect()
}

fn observed_mount_rows(status: &DaemonStatus, desired: &BTreeSet<String>) -> Vec<MountStatus> {
    status
        .mounts
        .iter()
        .filter(|mount| !desired.contains(&mount.mount))
        .map(|mount| MountStatus {
            name: mount.mount.clone(),
            root: PathBuf::from(format!("/{}", mount.mount.trim_start_matches('/'))),
            provider: ProviderPin {
                name: mount.provider_name.clone(),
                version: None,
                artifact: mount.provider_id.clone(),
            },
            auth: AuthState::NotNeeded,
            serving: ServingState::Live,
            access_count: 0,
            fix: None,
        })
        .collect()
}

/// Join one desired mount with daemon observations. A reachable daemon is not
/// evidence that every spec converged: only the explicit loaded mount list is
/// authoritative.
#[derive(Clone, Copy)]
enum Presence {
    Present,
    Absent,
}

#[derive(Clone, Copy)]
enum Health {
    Healthy,
    Unhealthy,
}

#[derive(Clone, Copy)]
struct MountObservation {
    provider: Presence,
    daemon: Presence,
    loaded: Presence,
    health: Health,
}

fn derive_serving_state(observation: MountObservation) -> ServingState {
    if matches!(observation.provider, Presence::Absent) {
        return ServingState::NotLoaded;
    }
    if matches!(observation.health, Health::Unhealthy) {
        return ServingState::Failed {
            message: "daemon health is unhealthy".into(),
        };
    }
    if matches!(observation.loaded, Presence::Present) {
        ServingState::Live
    } else if matches!(observation.daemon, Presence::Present) {
        ServingState::NotLoaded
    } else {
        ServingState::Offline
    }
}

fn provider_statuses(registry: &Registry, catalog: &Catalog) -> Result<Vec<ProviderStatus>> {
    let pins = reverse_pins(registry);
    let installed = catalog
        .installed()?
        .into_iter()
        .filter(|provider| provider.wasm_path().is_file())
        .map(|provider| InstalledProvider {
            name: provider.meta.name.to_string(),
            version: provider.meta.version.as_ref().map(ToString::to_string),
            artifact: provider.id.to_string(),
        })
        .collect();
    Ok(provider_rows(installed, pins))
}

struct InstalledProvider {
    name: String,
    version: Option<String>,
    artifact: String,
}

fn reverse_pins(
    registry: &Registry,
) -> BTreeMap<String, (String, Option<String>, BTreeSet<String>)> {
    let mut pins = BTreeMap::<String, (String, Option<String>, BTreeSet<String>)>::new();
    for (_, spec) in registry.iter() {
        let artifact = spec.provider.id.to_string();
        let entry = pins.entry(artifact).or_insert_with(|| {
            (
                spec.provider.meta.name.to_string(),
                spec.provider.meta.version.as_ref().map(ToString::to_string),
                BTreeSet::new(),
            )
        });
        entry.2.insert(spec.mount.clone());
    }
    pins
}

fn provider_rows(
    installed: Vec<InstalledProvider>,
    mut pins: BTreeMap<String, (String, Option<String>, BTreeSet<String>)>,
) -> Vec<ProviderStatus> {
    let mut rows = installed
        .into_iter()
        .map(|provider| {
            let artifact = provider.artifact;
            let pinned_by = pins
                .remove(&artifact)
                .map_or_else(BTreeSet::new, |entry| entry.2);
            let state = if pinned_by.is_empty() {
                ProviderState::Installed
            } else {
                ProviderState::Pinned
            };
            ProviderStatus {
                name: provider.name,
                version: provider.version,
                artifact,
                pinned_by: pinned_by.into_iter().collect(),
                state,
                fix: state.fix().map(str::to_owned),
            }
        })
        .collect::<Vec<_>>();
    for (artifact, (name, version, mounts)) in pins {
        rows.push(ProviderStatus {
            name,
            version,
            artifact,
            pinned_by: mounts.into_iter().collect(),
            state: ProviderState::Missing,
            fix: ProviderState::Missing.fix().map(str::to_owned),
        });
    }
    rows.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.version.cmp(&right.version))
            .then_with(|| left.artifact.cmp(&right.artifact))
    });
    rows
}

fn fs_type(value: Filesystem) -> FsType {
    match value {
        Filesystem::Fuse => FsType::Fuse,
        Filesystem::Nfs => FsType::Nfs,
    }
}
fn filesystem(value: FsType) -> Filesystem {
    match value {
        FsType::Fuse => Filesystem::Fuse,
        FsType::Nfs => Filesystem::Nfs,
    }
}
fn delivery(value: Environment) -> FrontendDelivery {
    match value {
        Environment::Host => FrontendDelivery::Local,
        Environment::Docker => FrontendDelivery::Docker,
        Environment::Krunkit => FrontendDelivery::Krunkit,
    }
}
fn environment(value: FrontendDelivery) -> Environment {
    match value {
        FrontendDelivery::Local => Environment::Host,
        FrontendDelivery::Docker => Environment::Docker,
        FrontendDelivery::Krunkit => Environment::Krunkit,
    }
}
fn guest_location(value: Environment) -> Option<&'static str> {
    match value {
        Environment::Docker | Environment::Krunkit => Some("/omnifs"),
        Environment::Host => None,
    }
}
fn frontend_cmp(left: &FrontendStatus, right: &FrontendStatus) -> Ordering {
    environment_rank(left.environment)
        .cmp(&environment_rank(right.environment))
        .then_with(|| left.filesystem.label().cmp(right.filesystem.label()))
        .then_with(|| left.location.cmp(&right.location))
}

fn environment_rank(environment: Environment) -> u8 {
    match environment {
        Environment::Host => 0,
        Environment::Docker => 1,
        Environment::Krunkit => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnifs_mtab::StateFile;
    use omnifs_workspace::config::PlanSource;

    #[test]
    fn human_state_labels_are_readable_without_changing_wire_names() {
        assert_eq!(AuthState::NotNeeded.severity(), Severity::Neutral);
        assert_eq!(AuthState::NotNeeded.label(), "not needed");
        assert_eq!(ServingState::NotLoaded.label(), "not loaded");
        assert_eq!(AccessState::FrontendStopped.label(), "frontend stopped");

        assert_eq!(
            serde_json::to_value(AuthState::NotNeeded).unwrap()["state"],
            "not_needed"
        );
        assert_eq!(
            serde_json::to_value(ServingState::NotLoaded).unwrap()["state"],
            "not_loaded"
        );
        assert_eq!(
            serde_json::to_value(AccessState::FrontendStopped).unwrap(),
            "frontend_stopped"
        );
    }

    #[test]
    fn auth_and_serving_precedence_preserves_fixes() {
        let auth = AuthState::Expired {
            command: "omnifs mount reauth x".into(),
        };
        let mount = MountStatus {
            name: "x".into(),
            root: "/x".into(),
            provider: ProviderPin {
                name: "p".into(),
                version: None,
                artifact: "a".repeat(64),
            },
            auth: auth.clone(),
            serving: ServingState::Offline,
            access_count: 0,
            fix: auth.command().map(ToOwned::to_owned),
        };
        let inventory = Inventory {
            workspace: WorkspaceStatus {
                home: "/home/.omnifs".into(),
                daemon: DaemonState::Stopped,
                namespace: NamespaceState::Offline,
                pid: None,
                api: None,
                runtime_expected: false,
            },
            frontends: vec![],
            mounts: vec![mount],
            providers: vec![],
        };
        assert_eq!(inventory.verdict(), Verdict::Degraded);
        assert_eq!(
            inventory.mounts[0].auth.command(),
            Some("omnifs mount reauth x")
        );
    }

    #[test]
    fn frontend_rows_are_all_namespace_and_sorted() {
        let rows = frontend_statuses(
            &[
                EffectiveFrontend {
                    filesystem: Filesystem::Fuse,
                    environment: Environment::Docker,
                    location: None,
                    source: PlanSource::Configured,
                },
                EffectiveFrontend {
                    filesystem: Filesystem::Nfs,
                    environment: Environment::Host,
                    location: Some("/z".into()),
                    source: PlanSource::PlatformDefault,
                },
                EffectiveFrontend {
                    filesystem: Filesystem::Fuse,
                    environment: Environment::Krunkit,
                    location: None,
                    source: PlanSource::Configured,
                },
            ],
            None,
            3,
            &[],
        );
        assert_eq!(rows[0].environment, Environment::Host);
        assert_eq!(rows[0].mount_count, 3);
        assert_eq!(rows[1].environment, Environment::Docker);
        assert_eq!(rows[2].environment, Environment::Krunkit);
        assert!(rows.iter().all(|row| row.scope == "all"));
    }

    #[test]
    fn access_paths_are_derived_on_request() {
        let inventory = Inventory {
            workspace: WorkspaceStatus {
                home: "/h".into(),
                daemon: DaemonState::Running,
                namespace: NamespaceState::Serving,
                pid: Some(1),
                api: None,
                runtime_expected: false,
            },
            frontends: vec![FrontendStatus {
                filesystem: Filesystem::Fuse,
                environment: Environment::Host,
                location: Some("/mnt".into()),
                source: FrontendSource::Configured,
                state: FrontendState::Attached,
                scope: "all",
                mount_count: 1,
                fix: None,
            }],
            mounts: vec![MountStatus {
                name: "github".into(),
                root: "/github".into(),
                provider: ProviderPin {
                    name: "github".into(),
                    version: Some("1".into()),
                    artifact: "a".repeat(64),
                },
                auth: AuthState::Ready,
                serving: ServingState::Live,
                access_count: 1,
                fix: None,
            }],
            providers: vec![],
        };
        let name = MountName::new("github").unwrap();
        assert_eq!(
            inventory.access_paths(&name)[0].path,
            PathBuf::from("/mnt/github")
        );
    }

    #[test]
    fn serving_state_matrix_joins_loaded_mounts() {
        assert_eq!(
            derive_serving_state(MountObservation {
                provider: Presence::Absent,
                daemon: Presence::Present,
                loaded: Presence::Absent,
                health: Health::Unhealthy,
            }),
            ServingState::NotLoaded,
            "missing artifact outranks daemon failure"
        );
        assert_eq!(
            derive_serving_state(MountObservation {
                provider: Presence::Present,
                daemon: Presence::Present,
                loaded: Presence::Absent,
                health: Health::Unhealthy,
            }),
            ServingState::Failed {
                message: "daemon health is unhealthy".into()
            }
        );
        assert_eq!(
            derive_serving_state(MountObservation {
                provider: Presence::Present,
                daemon: Presence::Present,
                loaded: Presence::Absent,
                health: Health::Healthy,
            }),
            ServingState::NotLoaded,
            "a reachable daemon does not imply every spec converged"
        );
        assert_eq!(
            derive_serving_state(MountObservation {
                provider: Presence::Present,
                daemon: Presence::Present,
                loaded: Presence::Present,
                health: Health::Healthy,
            }),
            ServingState::Live
        );
        assert_eq!(
            derive_serving_state(MountObservation {
                provider: Presence::Present,
                daemon: Presence::Absent,
                loaded: Presence::Absent,
                health: Health::Healthy,
            }),
            ServingState::Offline
        );
    }

    #[test]
    fn probe_failure_is_unreachable_only_when_runtime_expected() {
        let layout = WorkspaceLayout {
            config_dir: PathBuf::from("/home/.omnifs"),
            cache_dir: PathBuf::from("/home/.omnifs/cache"),
            mounts_dir: PathBuf::from("/home/.omnifs/mounts"),
            providers_dir: PathBuf::from("/home/.omnifs/providers"),
            credentials_file: PathBuf::from("/home/.omnifs/credentials.json"),
            config_file: PathBuf::from("/home/.omnifs/config.toml"),
        };
        let probe = Err(anyhow::anyhow!("connection refused"));
        let expected = RuntimeRecord::new(
            omnifs_workspace::mounts::Revision::new("a".repeat(40)).unwrap(),
            omnifs_workspace::runtime_record::Endpoint::Unix {
                path: PathBuf::from("/home/.omnifs/frontends/local.sock"),
            },
            omnifs_workspace::runtime_record::RecordedBackend::Native { pid: 42 },
            "instance".into(),
            Vec::new(),
        );
        assert_eq!(
            workspace_status(&layout, None, Some(&expected), &probe).daemon,
            DaemonState::Unreachable
        );
        assert_eq!(
            workspace_status(&layout, None, None, &probe).daemon,
            DaemonState::Stopped
        );
    }

    #[test]
    fn daemon_health_maps_to_distinct_operational_states() {
        let layout = WorkspaceLayout::under_root(Path::new("/home/.omnifs"));
        for (health, expected) in [
            (HealthState::Healthy, DaemonState::Running),
            (HealthState::Starting, DaemonState::Starting),
            (HealthState::Degraded, DaemonState::Degraded),
            (HealthState::Unhealthy, DaemonState::Failed),
        ] {
            let status = DaemonStatus {
                version: "test".into(),
                api_major: 1,
                api_minor: 0,
                pid: 1,
                instance_id: "instance".into(),
                executable: "/bin/omnifs".into(),
                config_dir: "/home/.omnifs".into(),
                cache_dir: "/home/.omnifs/cache".into(),
                providers_dir: "/home/.omnifs/providers".into(),
                frontends: Vec::new(),
                backend: omnifs_api::DaemonBackend::Native { pid: 1 },
                mounts: Vec::new(),
                health: omnifs_api::DaemonHealth::new(vec![omnifs_api::SubsystemHealth::new(
                    omnifs_api::DaemonSubsystem::Control,
                    health,
                    "test",
                )]),
            };
            assert_eq!(
                workspace_status(&layout, Some(&status), None, &Ok(None)).daemon,
                expected
            );
        }
    }

    #[test]
    fn access_paths_cover_every_frontend_and_mount_state() {
        let inventory = Inventory {
            workspace: WorkspaceStatus {
                home: "/h".into(),
                daemon: DaemonState::Running,
                namespace: NamespaceState::Serving,
                pid: Some(1),
                api: None,
                runtime_expected: true,
            },
            frontends: vec![
                FrontendStatus {
                    filesystem: Filesystem::Fuse,
                    environment: Environment::Host,
                    location: Some("/host".into()),
                    source: FrontendSource::Configured,
                    state: FrontendState::Attached,
                    scope: "all",
                    mount_count: 1,
                    fix: None,
                },
                FrontendStatus {
                    filesystem: Filesystem::Fuse,
                    environment: Environment::Docker,
                    location: Some("/omnifs".into()),
                    source: FrontendSource::Configured,
                    state: FrontendState::Unmanaged,
                    scope: "all",
                    mount_count: 1,
                    fix: None,
                },
            ],
            mounts: vec![MountStatus {
                name: "github".into(),
                root: "/github".into(),
                provider: ProviderPin {
                    name: "github".into(),
                    version: None,
                    artifact: "a".repeat(64),
                },
                auth: AuthState::Ready,
                serving: ServingState::Live,
                access_count: 1,
                fix: None,
            }],
            providers: vec![],
        };
        let paths = inventory.access_paths(&MountName::new("github").unwrap());
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].path, PathBuf::from("/host/github"));
        assert_eq!(paths[0].state, AccessState::Available);
        assert_eq!(paths[1].path, PathBuf::from("/omnifs/github"));
        assert_eq!(paths[1].state, AccessState::Available);
    }

    #[test]
    fn daemon_down_keeps_runner_owned_local_frontend_visible() {
        let tmp = tempfile::TempDir::new().unwrap();
        let layout = WorkspaceLayout::under_root(tmp.path());
        let mount_point = tmp.path().join("mounted");
        let state_dir = layout.frontend_state_dir(
            omnifs_workspace::runtime_record::FrontendKind::Fuse,
            &mount_point,
        );
        let _guard = StateFile::write_fuse(&mount_point, &state_dir).unwrap();
        let effective = vec![EffectiveFrontend {
            filesystem: Filesystem::Fuse,
            environment: Environment::Host,
            location: Some(mount_point.clone()),
            source: PlanSource::Configured,
        }];
        let fallback = local_frontend_fallback(&layout, &effective, 1).unwrap();
        let rows = frontend_statuses(&effective, None, 1, &fallback);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, FrontendState::Attached);
        assert_eq!(rows[0].location, Some(mount_point));
    }

    #[test]
    fn verdict_matrix_maps_actionable_states() {
        let base = Inventory {
            workspace: WorkspaceStatus {
                home: "/h".into(),
                daemon: DaemonState::Stopped,
                namespace: NamespaceState::Offline,
                pid: None,
                api: None,
                runtime_expected: false,
            },
            frontends: vec![],
            mounts: vec![MountStatus {
                name: "x".into(),
                root: "/x".into(),
                provider: ProviderPin {
                    name: "p".into(),
                    version: None,
                    artifact: "a".repeat(64),
                },
                auth: AuthState::Ready,
                serving: ServingState::Offline,
                access_count: 0,
                fix: None,
            }],
            providers: vec![],
        };
        assert_eq!(
            base.verdict(),
            Verdict::Ok,
            "deliberately stopped is neutral"
        );
        let mut expired = base.clone();
        expired.mounts[0].auth = AuthState::Expired {
            command: "omnifs mount reauth x".into(),
        };
        assert_eq!(expired.verdict(), Verdict::Degraded);
        let mut unmanaged = base.clone();
        unmanaged.workspace.daemon = DaemonState::Running;
        unmanaged.frontends.push(FrontendStatus {
            filesystem: Filesystem::Fuse,
            environment: Environment::Host,
            location: Some("/mnt".into()),
            source: FrontendSource::Unmanaged,
            state: FrontendState::Unmanaged,
            scope: "all",
            mount_count: 1,
            fix: Some("omnifs frontend disable".into()),
        });
        assert_eq!(unmanaged.verdict(), Verdict::Degraded);
        let mut unreachable = base;
        unreachable.workspace.daemon = DaemonState::Unreachable;
        unreachable.workspace.runtime_expected = true;
        assert_eq!(unreachable.verdict(), Verdict::Degraded);
    }

    #[test]
    fn structured_inventory_keeps_runtime_expectation_and_absolute_identity() {
        let inventory = Inventory {
            workspace: WorkspaceStatus {
                home: "/home/.omnifs".into(),
                daemon: DaemonState::Stopped,
                namespace: NamespaceState::Offline,
                pid: None,
                api: None,
                runtime_expected: true,
            },
            frontends: vec![],
            mounts: vec![MountStatus {
                name: "x".into(),
                root: "/x".into(),
                provider: ProviderPin {
                    name: "p".into(),
                    version: Some("1.2.3".into()),
                    artifact: "b".repeat(64),
                },
                auth: AuthState::NotNeeded,
                serving: ServingState::Offline,
                access_count: 0,
                fix: None,
            }],
            providers: vec![],
        };
        let json = serde_json::to_value(inventory).unwrap();
        assert_eq!(json["workspace"]["runtime_expected"], true);
        assert_eq!(json["mounts"][0]["root"], "/x");
        assert_eq!(
            json["mounts"][0]["provider"]["artifact"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
    }

    #[test]
    fn provider_rows_reverse_pin_exact_artifacts_and_sort_mounts() {
        let installed_artifact = "a".repeat(64);
        let missing_artifact = "b".repeat(64);
        let mut pins = BTreeMap::new();
        pins.insert(
            installed_artifact.clone(),
            (
                "github".into(),
                Some("1.0.0".into()),
                ["zeta", "alpha"].into_iter().map(str::to_owned).collect(),
            ),
        );
        pins.insert(
            missing_artifact.clone(),
            (
                "github".into(),
                Some("2.0.0".into()),
                BTreeSet::from(["work".into()]),
            ),
        );
        let rows = provider_rows(
            vec![InstalledProvider {
                name: "github".into(),
                version: Some("1.0.0".into()),
                artifact: installed_artifact.clone(),
            }],
            pins,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].state, ProviderState::Pinned);
        assert_eq!(rows[0].pinned_by, vec!["alpha", "zeta"]);
        assert_eq!(rows[0].artifact, installed_artifact);
        assert_eq!(rows[1].state, ProviderState::Missing);
        assert_eq!(rows[1].artifact, missing_artifact);
        assert_eq!(rows[1].fix.as_deref(), Some("omnifs provider add <path>"));
        assert_eq!(ProviderState::Installed.severity(), Severity::Neutral);
        assert_eq!(ProviderState::Missing.severity(), Severity::Attention);
    }

    #[test]
    fn frontend_rows_distinguish_unattached_and_unmanaged_observations() {
        let effective = vec![
            EffectiveFrontend {
                filesystem: Filesystem::Fuse,
                environment: Environment::Host,
                location: Some("/host".into()),
                source: PlanSource::Configured,
            },
            EffectiveFrontend {
                filesystem: Filesystem::Fuse,
                environment: Environment::Docker,
                location: None,
                source: PlanSource::Configured,
            },
        ];
        let daemon = DaemonStatus {
            version: "test".into(),
            api_major: 1,
            api_minor: 0,
            pid: 1,
            instance_id: "instance".into(),
            executable: "/bin/omnifs".into(),
            config_dir: "/home/.omnifs".into(),
            cache_dir: "/home/.omnifs/cache".into(),
            providers_dir: "/home/.omnifs/providers".into(),
            frontends: vec![
                omnifs_api::FrontendInfo {
                    source: "host".into(),
                    fs_type: FsType::Fuse,
                    mount_point: "/host".into(),
                    delivery: FrontendDelivery::Local,
                },
                omnifs_api::FrontendInfo {
                    source: "other".into(),
                    fs_type: FsType::Nfs,
                    mount_point: "/other".into(),
                    delivery: FrontendDelivery::Local,
                },
            ],
            backend: omnifs_api::DaemonBackend::Native { pid: 1 },
            mounts: vec![],
            health: omnifs_api::DaemonHealth::default(),
        };
        let rows = frontend_statuses(&effective, Some(&daemon), 4, &[]);
        assert_eq!(rows.iter().filter(|row| row.scope == "all").count(), 3);
        assert!(rows.iter().any(|row| {
            row.environment == Environment::Docker && row.state == FrontendState::Unattached
        }));
        assert!(rows.iter().any(|row| {
            row.filesystem == Filesystem::Nfs
                && row.environment == Environment::Host
                && row.state == FrontendState::Unmanaged
        }));
        assert!(rows.iter().all(|row| row.mount_count == 4));
    }
}
