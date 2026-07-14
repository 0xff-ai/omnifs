//! Authoritative application inventory.
//!
//! This module owns the typed facts consumed by status, list, and receipt
//! surfaces. Collection is deliberately at the edge; all joins, sorting, and
//! verdict decisions below are pure.

use anyhow::Result;
use omnifs_api::{DaemonStatus, FrontendDelivery, FsType, HealthState};
use omnifs_mtab::{MountKind, MountState};
use omnifs_workspace::creds::FileStore;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::mounts::{Name as MountName, Registry, Revision};
use omnifs_workspace::provider::Catalog;
use omnifs_workspace::runtime_record::RuntimeRecord;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::auth::{AuthReadiness, MountAuth};
use crate::commands::frontend::{
    FrontendEnvironment as Environment, FrontendFilesystem as Filesystem,
};
use crate::mount_config::MountConfig;
use crate::workspace::Workspace;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Inventory {
    pub(crate) home: PathBuf,
    pub(crate) mount_revision: Option<Revision>,
    pub(crate) applied_revision: Option<Revision>,
    #[serde(skip_serializing)]
    pub(crate) desired_mounts: Vec<MountConfig>,
    pub(crate) daemon: DaemonObservation,
    pub(crate) runners: Vec<RunnerStatus>,
    pub(crate) frontends: Vec<FrontendStatus>,
    pub(crate) mounts: Vec<MountStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DaemonObservation {
    pub(crate) status: Option<DaemonStatus>,
    pub(crate) probe: DaemonProbe,
    #[serde(skip_serializing)]
    pub(crate) runtime: Option<RuntimeRecord>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum DaemonProbe {
    Responding,
    Stopped,
    Unreachable { message: String },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct RunnerStatus {
    pub(crate) filesystem: Filesystem,
    pub(crate) location: Option<PathBuf>,
    pub(crate) state: RunnerState,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunnerState {
    Attached,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DaemonState {
    Running,
    Starting,
    Degraded,
    Stopped,
    Failed,
    Unreachable,
}

impl DaemonObservation {
    pub(crate) fn state(&self) -> DaemonState {
        match (&self.probe, self.status.as_ref()) {
            (DaemonProbe::Stopped, _) => DaemonState::Stopped,
            (DaemonProbe::Unreachable { .. }, _) => {
                if self.runtime.is_some() {
                    DaemonState::Unreachable
                } else {
                    DaemonState::Stopped
                }
            },
            (DaemonProbe::Responding, Some(status)) => match status.health.overall_state() {
                HealthState::Healthy => DaemonState::Running,
                HealthState::Starting => DaemonState::Starting,
                HealthState::Degraded => DaemonState::Degraded,
                HealthState::Unhealthy => DaemonState::Failed,
            },
            (DaemonProbe::Responding, None) => DaemonState::Unreachable,
        }
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.status.as_ref().map(|status| status.pid).or_else(|| {
            self.runtime.as_ref().map(|record| match record.backend {
                omnifs_workspace::runtime_record::RecordedBackend::Native { pid } => pid,
            })
        })
    }

    #[cfg(test)]
    pub(crate) fn test(state: DaemonState) -> Self {
        let probe = match state {
            DaemonState::Stopped => DaemonProbe::Stopped,
            DaemonState::Unreachable => DaemonProbe::Unreachable {
                message: "unreachable".to_owned(),
            },
            DaemonState::Running
            | DaemonState::Starting
            | DaemonState::Degraded
            | DaemonState::Failed => DaemonProbe::Responding,
        };
        let status = match state {
            DaemonState::Stopped | DaemonState::Unreachable => None,
            _ => Some(DaemonStatus {
                version: "test".to_owned(),
                pid: 1,
                instance_id: "test-instance".to_owned(),
                executable: "/bin/omnifs".into(),
                config_dir: "/tmp/omnifs".into(),
                cache_dir: "/tmp/omnifs/cache".into(),
                providers_dir: "/tmp/omnifs/providers".into(),
                frontends: Vec::new(),
                backend: omnifs_api::DaemonBackend::Native { pid: 1 },
                mounts: Vec::new(),
                health: omnifs_api::DaemonHealth::new(vec![omnifs_api::SubsystemHealth::new(
                    omnifs_api::DaemonSubsystem::Control,
                    match state {
                        DaemonState::Running | DaemonState::Stopped | DaemonState::Unreachable => {
                            HealthState::Healthy
                        },
                        DaemonState::Starting => HealthState::Starting,
                        DaemonState::Degraded => HealthState::Degraded,
                        DaemonState::Failed => HealthState::Unhealthy,
                    },
                    "test",
                )]),
            }),
        };
        let runtime = (state == DaemonState::Unreachable).then(|| {
            RuntimeRecord::new(
                Revision::new("a".repeat(40)).expect("test revision"),
                omnifs_workspace::runtime_record::Endpoint::Unix {
                    path: "/tmp/omnifs/control.sock".into(),
                },
                omnifs_workspace::runtime_record::RecordedBackend::Native { pid: 1 },
                "test-instance".to_owned(),
            )
        });
        Self {
            status,
            probe,
            runtime,
        }
    }
}

impl From<Result<Option<DaemonStatus>, anyhow::Error>> for DaemonObservation {
    fn from(probe: Result<Option<DaemonStatus>, anyhow::Error>) -> Self {
        match probe {
            Ok(Some(status)) => Self {
                status: Some(status),
                probe: DaemonProbe::Responding,
                runtime: None,
            },
            Ok(None) => Self {
                status: None,
                probe: DaemonProbe::Stopped,
                runtime: None,
            },
            Err(error) => Self {
                status: None,
                probe: DaemonProbe::Unreachable {
                    message: format!("{error:#}"),
                },
                runtime: None,
            },
        }
    }
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
    pub(crate) state: FrontendState,
    pub(crate) scope: &'static str,
    pub(crate) mount_count: usize,
    pub(crate) fix: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FrontendState {
    Attached,
    Running,
    Failed,
}

impl FrontendState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Running => "running",
            Self::Failed => "failed",
        }
    }

    pub(crate) const fn severity(self) -> Severity {
        match self {
            Self::Attached | Self::Running => Severity::Positive,
            Self::Failed => Severity::Error,
        }
    }

    pub(crate) const fn fix(self) -> Option<&'static str> {
        match self {
            Self::Failed => Some("omnifs logs"),
            Self::Attached | Self::Running => None,
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
    pub(crate) state: ProviderPinState,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ProviderPinState {
    Available,
    Missing,
    Corrupt { message: String },
}

impl ProviderPinState {
    pub(crate) const fn severity(&self) -> Severity {
        match self {
            Self::Available => Severity::Positive,
            Self::Missing => Severity::Attention,
            Self::Corrupt { .. } => Severity::Error,
        }
    }

    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Missing => "missing",
            Self::Corrupt { .. } => "corrupt",
        }
    }
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

    fn from_observed(observed: &omnifs_api::MountInfo) -> Self {
        let command = format!("omnifs mount reauth {}", observed.mount);
        match observed.auth_health {
            None => Self::NotNeeded,
            Some(
                omnifs_api::CredentialHealth::Ready
                | omnifs_api::CredentialHealth::ExpiringSoon
                | omnifs_api::CredentialHealth::StaticUnvalidated,
            ) => Self::Ready,
            Some(omnifs_api::CredentialHealth::Missing) => Self::Missing { command },
            Some(omnifs_api::CredentialHealth::Expired) => Self::Expired { command },
            Some(omnifs_api::CredentialHealth::RefreshFailed) => Self::Error {
                message: "credential refresh failed".into(),
                command,
            },
            Some(omnifs_api::CredentialHealth::NeedsConsent) => Self::Error {
                message: "credential needs consent".into(),
                command,
            },
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
    Offline,
    Failed,
}

impl AccessState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Offline => "offline",
            Self::Failed => "failed",
        }
    }
}

impl Inventory {
    pub(crate) async fn collect(workspace: &Workspace) -> Result<Self> {
        let layout = workspace.layout().clone();
        let repository = workspace.observe_repository()?;
        let registry = repository.registry();
        let mount_revision = repository.head_revision()?;
        let applied_revision = repository.applied()?;
        let catalog = workspace.catalog();
        let credentials = FileStore::new(&layout.credentials_file);
        let daemon_probe = workspace.daemon().status_optional_checked().await;
        let daemon_status = daemon_probe.as_ref().ok().and_then(Option::as_ref);
        let runtime = RuntimeRecord::read(&layout.runtime_record_file())
            .ok()
            .flatten();
        let mut mounts = mount_statuses(registry, catalog, &credentials, daemon_status);
        let mount_count = mounts.len();
        let runners = runner_statuses(&layout)?;
        let frontends = frontend_statuses(daemon_status, mount_count, &runners);
        let access_count = frontends
            .iter()
            .filter(|frontend| {
                matches!(
                    frontend.state,
                    FrontendState::Attached | FrontendState::Running
                )
            })
            .count();
        for mount in &mut mounts {
            mount.access_count = access_count;
        }
        let desired_mounts = registry
            .iter()
            .map(|(name, spec)| MountConfig {
                name: name.clone(),
                config: spec.clone(),
                source: registry.spec_path(name),
            })
            .collect();
        let mut daemon = DaemonObservation::from(daemon_probe);
        daemon.runtime = runtime;
        Ok(Self {
            home: layout.config_dir,
            mount_revision,
            applied_revision,
            desired_mounts,
            daemon,
            runners,
            frontends,
            mounts,
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
                    FrontendState::Attached | FrontendState::Running => {
                        match mount_status.serving {
                            ServingState::Live => AccessState::Available,
                            ServingState::Failed { .. } => AccessState::Failed,
                            ServingState::Offline | ServingState::NotLoaded => AccessState::Offline,
                        }
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
                    self.daemon.state(),
                    DaemonState::Running | DaemonState::Starting | DaemonState::Degraded
                )
        }) || self.mounts.iter().any(|entry| {
            entry.fix.is_some()
                || entry.provider.state.severity() >= Severity::Attention
                || entry.auth.severity() >= Severity::Attention
                || entry.serving.severity() >= Severity::Attention
        }) || matches!(
            self.daemon.state(),
            DaemonState::Failed | DaemonState::Unreachable
        );
        if degraded {
            Verdict::Degraded
        } else {
            Verdict::Ok
        }
    }

    pub(crate) fn daemon_state(&self) -> DaemonState {
        self.daemon.state()
    }

    #[cfg(test)]
    pub(crate) fn test(
        state: DaemonState,
        frontends: Vec<FrontendStatus>,
        mounts: Vec<MountStatus>,
    ) -> Self {
        Self {
            home: PathBuf::from("/tmp/omnifs"),
            mount_revision: None,
            applied_revision: None,
            desired_mounts: Vec::new(),
            daemon: DaemonObservation::test(state),
            runners: Vec::new(),
            frontends,
            mounts,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Verdict {
    Ok,
    Degraded,
}

/// Discover host-owned runner records when the daemon cannot answer. These
/// records are the runner's observable access surface, so a daemon-down status
/// must retain them instead of reporting every local frontend as stopped.
fn runner_statuses(layout: &WorkspaceLayout) -> Result<Vec<RunnerStatus>> {
    let mut rows = Vec::new();
    for path in MountState::files_under(&layout.frontend_state_root())? {
        match MountState::read_file(&path) {
            Ok(state) => {
                let filesystem = match state.kind {
                    MountKind::Fuse => Filesystem::Fuse,
                    MountKind::Nfs { .. } => Filesystem::Nfs,
                };
                rows.push(RunnerStatus {
                    filesystem,
                    location: Some(state.mount_point),
                    state: RunnerState::Attached,
                    error: None,
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
                rows.push(RunnerStatus {
                    filesystem,
                    location: None,
                    state: RunnerState::Failed,
                    error: Some(error.to_string()),
                });
            },
        }
    }
    Ok(rows)
}

fn frontend_statuses(
    daemon: Option<&DaemonStatus>,
    mount_count: usize,
    runners: &[RunnerStatus],
) -> Vec<FrontendStatus> {
    let mut rows = daemon
        .into_iter()
        .flat_map(|status| status.frontends.iter())
        .map(|observed| {
            let state = if daemon
                .is_some_and(|status| status.health.overall_state() == HealthState::Unhealthy)
            {
                FrontendState::Failed
            } else {
                FrontendState::Attached
            };
            FrontendStatus {
                filesystem: filesystem(observed.fs_type),
                environment: environment(observed.delivery),
                location: Some(observed.mount_point.clone()),
                state,
                scope: "all",
                mount_count,
                fix: state.fix().map(str::to_owned),
            }
        })
        .collect::<Vec<_>>();

    for runner in runners {
        let matched = rows.iter().any(|row| {
            row.filesystem == runner.filesystem
                && row.environment == Environment::Host
                && row.location == runner.location
        });
        if daemon.is_none() || !matched {
            let state = match runner.state {
                RunnerState::Attached => FrontendState::Running,
                RunnerState::Failed => FrontendState::Failed,
            };
            rows.push(FrontendStatus {
                filesystem: runner.filesystem,
                environment: Environment::Host,
                location: runner.location.clone(),
                state,
                scope: "all",
                mount_count,
                fix: runner.error.as_ref().map_or_else(
                    || state.fix().map(str::to_owned),
                    |error| Some(format!("omnifs logs ({error})")),
                ),
            });
        }
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
                artifact: artifact.clone(),
                state: match catalog.get(&spec.provider.id) {
                    Ok(Some(_)) => ProviderPinState::Available,
                    Ok(None) => ProviderPinState::Missing,
                    Err(error) => ProviderPinState::Corrupt {
                        message: error.to_string(),
                    },
                },
            };
            let local_auth = AuthState::from_readiness(
                &MountAuth::from_spec(catalog, spec.clone()).readiness(credentials),
                &name_string,
            );
            let auth = mount_auth_state(&name_string, local_auth, daemon);
            let provider_present = matches!(provider.state, ProviderPinState::Available);
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
            let fix = if let Some(command) = auth.command() {
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

fn mount_auth_state(mount: &str, local: AuthState, daemon: Option<&DaemonStatus>) -> AuthState {
    let Some(observed) =
        daemon.and_then(|status| status.mounts.iter().find(|entry| entry.mount == mount))
    else {
        return local;
    };

    AuthState::from_observed(observed)
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
                state: ProviderPinState::Corrupt {
                    message: failure.error.to_string(),
                },
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
        .map(|mount| {
            let auth = AuthState::from_observed(mount);
            let fix = auth.command().map(str::to_owned);
            MountStatus {
                name: mount.mount.clone(),
                root: PathBuf::from(format!("/{}", mount.mount.trim_start_matches('/'))),
                provider: ProviderPin {
                    name: mount.provider_name.clone(),
                    version: None,
                    artifact: mount.provider_id.clone(),
                    state: ProviderPinState::Available,
                },
                auth,
                serving: ServingState::Live,
                access_count: 0,
                fix,
            }
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

fn filesystem(value: FsType) -> Filesystem {
    match value {
        FsType::Fuse => Filesystem::Fuse,
        FsType::Nfs => Filesystem::Nfs,
    }
}
fn environment(value: FrontendDelivery) -> Environment {
    match value {
        FrontendDelivery::Local => Environment::Host,
        FrontendDelivery::Docker => Environment::Docker,
        FrontendDelivery::Krunkit => Environment::Krunkit,
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

    #[test]
    fn human_state_labels_are_readable_without_changing_wire_names() {
        assert_eq!(AuthState::NotNeeded.severity(), Severity::Neutral);
        assert_eq!(AuthState::NotNeeded.label(), "not needed");
        assert_eq!(ServingState::NotLoaded.label(), "not loaded");

        assert_eq!(
            serde_json::to_value(AuthState::NotNeeded).unwrap()["state"],
            "not_needed"
        );
        assert_eq!(
            serde_json::to_value(ServingState::NotLoaded).unwrap()["state"],
            "not_loaded"
        );
    }

    #[test]
    fn live_daemon_auth_health_overrides_fresh_local_store_readiness() {
        let mut observation = DaemonObservation::test(DaemonState::Running);
        observation.status.as_mut().unwrap().mounts = vec![
            omnifs_api::MountInfo {
                mount: "consent".into(),
                provider_name: "test".into(),
                provider_id: "a".repeat(64),
                auth_health: Some(omnifs_api::CredentialHealth::NeedsConsent),
            },
            omnifs_api::MountInfo {
                mount: "refresh".into(),
                provider_name: "test".into(),
                provider_id: "b".repeat(64),
                auth_health: Some(omnifs_api::CredentialHealth::RefreshFailed),
            },
        ];
        let daemon = observation.status.as_ref();

        let consent = mount_auth_state("consent", AuthState::Ready, daemon);
        assert!(matches!(consent, AuthState::Error { .. }));
        assert_eq!(consent.command(), Some("omnifs mount reauth consent"));

        let refresh = mount_auth_state("refresh", AuthState::Ready, daemon);
        assert!(matches!(refresh, AuthState::Error { .. }));
        assert_eq!(refresh.command(), Some("omnifs mount reauth refresh"));

        assert_eq!(
            mount_auth_state("unobserved", AuthState::Ready, daemon),
            AuthState::Ready,
            "local readiness is only a fallback when the daemon has no row"
        );

        let rows = observed_mount_rows(daemon.unwrap(), &BTreeSet::new());
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| matches!(row.auth, AuthState::Error { .. }))
        );
        assert!(rows.iter().all(|row| {
            row.fix
                .as_deref()
                .is_some_and(|fix| fix.starts_with("omnifs mount reauth "))
        }));
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
                state: ProviderPinState::Available,
            },
            auth: auth.clone(),
            serving: ServingState::Offline,
            access_count: 0,
            fix: auth.command().map(ToOwned::to_owned),
        };
        let inventory = Inventory::test(DaemonState::Stopped, vec![], vec![mount]);
        assert_eq!(inventory.verdict(), Verdict::Degraded);
        assert_eq!(
            inventory.mounts[0].auth.command(),
            Some("omnifs mount reauth x")
        );
    }

    #[test]
    fn access_paths_are_derived_on_request() {
        let inventory = Inventory::test(
            DaemonState::Running,
            vec![FrontendStatus {
                filesystem: Filesystem::Fuse,
                environment: Environment::Host,
                location: Some("/mnt".into()),
                state: FrontendState::Attached,
                scope: "all",
                mount_count: 1,
                fix: None,
            }],
            vec![MountStatus {
                name: "github".into(),
                root: "/github".into(),
                provider: ProviderPin {
                    name: "github".into(),
                    version: Some("1".into()),
                    artifact: "a".repeat(64),
                    state: ProviderPinState::Available,
                },
                auth: AuthState::Ready,
                serving: ServingState::Live,
                access_count: 1,
                fix: None,
            }],
        );
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
        let probe = Err(anyhow::anyhow!("connection refused"));
        let expected = RuntimeRecord::new(
            omnifs_workspace::mounts::Revision::new("a".repeat(40)).unwrap(),
            omnifs_workspace::runtime_record::Endpoint::Unix {
                path: PathBuf::from("/home/.omnifs/frontends/local.sock"),
            },
            omnifs_workspace::runtime_record::RecordedBackend::Native { pid: 42 },
            "instance".into(),
        );
        let mut unreachable = DaemonObservation::from(probe);
        unreachable.runtime = Some(expected);
        assert_eq!(unreachable.state(), DaemonState::Unreachable);
        assert_eq!(
            DaemonObservation::from(Err(anyhow::anyhow!("connection refused"))).state(),
            DaemonState::Stopped
        );
    }

    #[test]
    fn daemon_health_maps_to_distinct_operational_states() {
        for (health, expected) in [
            (HealthState::Healthy, DaemonState::Running),
            (HealthState::Starting, DaemonState::Starting),
            (HealthState::Degraded, DaemonState::Degraded),
            (HealthState::Unhealthy, DaemonState::Failed),
        ] {
            let status = DaemonStatus {
                version: "test".into(),
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
            assert_eq!(DaemonObservation::from(Ok(Some(status))).state(), expected);
        }
    }

    #[test]
    fn access_paths_cover_every_frontend_and_mount_state() {
        let inventory = Inventory::test(
            DaemonState::Running,
            vec![
                FrontendStatus {
                    filesystem: Filesystem::Fuse,
                    environment: Environment::Host,
                    location: Some("/host".into()),
                    state: FrontendState::Attached,
                    scope: "all",
                    mount_count: 1,
                    fix: None,
                },
                FrontendStatus {
                    filesystem: Filesystem::Fuse,
                    environment: Environment::Docker,
                    location: Some("/omnifs".into()),
                    state: FrontendState::Attached,
                    scope: "all",
                    mount_count: 1,
                    fix: None,
                },
            ],
            vec![MountStatus {
                name: "github".into(),
                root: "/github".into(),
                provider: ProviderPin {
                    name: "github".into(),
                    version: None,
                    artifact: "a".repeat(64),
                    state: ProviderPinState::Available,
                },
                auth: AuthState::Ready,
                serving: ServingState::Live,
                access_count: 1,
                fix: None,
            }],
        );
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
        let fallback = runner_statuses(&layout).unwrap();
        let rows = frontend_statuses(None, 1, &fallback);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, FrontendState::Running);
        assert_eq!(rows[0].location, Some(mount_point));
    }

    #[test]
    fn verdict_matrix_maps_actionable_states() {
        let base = Inventory::test(
            DaemonState::Stopped,
            vec![],
            vec![MountStatus {
                name: "x".into(),
                root: "/x".into(),
                provider: ProviderPin {
                    name: "p".into(),
                    version: None,
                    artifact: "a".repeat(64),
                    state: ProviderPinState::Available,
                },
                auth: AuthState::Ready,
                serving: ServingState::Offline,
                access_count: 0,
                fix: None,
            }],
        );
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
        unmanaged.daemon = DaemonObservation::test(DaemonState::Running);
        unmanaged.frontends.push(FrontendStatus {
            filesystem: Filesystem::Fuse,
            environment: Environment::Host,
            location: Some("/mnt".into()),
            state: FrontendState::Failed,
            scope: "all",
            mount_count: 1,
            fix: Some("omnifs frontend disable".into()),
        });
        assert_eq!(unmanaged.verdict(), Verdict::Degraded);
        let mut unreachable = base;
        unreachable.daemon = DaemonObservation::test(DaemonState::Unreachable);
        assert_eq!(unreachable.verdict(), Verdict::Degraded);
    }

    #[test]
    fn structured_inventory_keeps_runtime_expectation_and_absolute_identity() {
        let inventory = Inventory::test(
            DaemonState::Stopped,
            vec![],
            vec![MountStatus {
                name: "x".into(),
                root: "/x".into(),
                provider: ProviderPin {
                    name: "p".into(),
                    version: Some("1.2.3".into()),
                    artifact: "b".repeat(64),
                    state: ProviderPinState::Available,
                },
                auth: AuthState::NotNeeded,
                serving: ServingState::Offline,
                access_count: 0,
                fix: None,
            }],
        );
        let json = serde_json::to_value(inventory).unwrap();
        assert_eq!(json["daemon"]["probe"]["state"], "stopped");
        assert_eq!(json["mounts"][0]["root"], "/x");
        assert_eq!(
            json["mounts"][0]["provider"]["artifact"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
    }
}
