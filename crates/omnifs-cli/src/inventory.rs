//! Authoritative application inventory.
//!
//! This module owns the typed facts consumed by status, list, and receipt
//! surfaces. Collection is deliberately at the edge; all joins, sorting, and
//! verdict decisions below are pure.

use anyhow::Result;
use omnifs_api::{DaemonStatus, HealthState};
use omnifs_core::{MountName, fs};
use omnifs_workspace::creds::FileStore;
use omnifs_workspace::daemon_record::DaemonRecord;
use omnifs_workspace::mounts::{Registry, Revision};
use omnifs_workspace::provider::Catalog;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::auth::{AuthReadiness, MountAuth};
use crate::provider_warmup::WarmupStatus;
use omnifs_workspace::Workspace;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Inventory {
    pub(crate) home: PathBuf,
    pub(crate) mount_revision: Option<Revision>,
    pub(crate) applied_revision: Option<Revision>,
    pub(crate) daemon: DaemonFacts,
    pub(crate) filesystems: Vec<FilesystemStatus>,
    pub(crate) mounts: Vec<MountStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) warmup: Option<WarmupStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DaemonFacts {
    pub(crate) status: Option<DaemonStatus>,
    pub(crate) probe: DaemonProbe,
    #[serde(skip_serializing)]
    pub(crate) runtime: Option<DaemonRecord>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum DaemonProbe {
    Responding,
    Stopped,
    Unreachable { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DaemonHealth {
    Running,
    Starting,
    Degraded,
    Stopped,
    Failed,
    Unreachable,
}

impl DaemonFacts {
    pub(crate) fn health(&self) -> DaemonHealth {
        match (&self.probe, self.status.as_ref()) {
            (DaemonProbe::Stopped, _) => DaemonHealth::Stopped,
            (DaemonProbe::Unreachable { .. }, _) => {
                if self.runtime.is_some() {
                    DaemonHealth::Unreachable
                } else {
                    DaemonHealth::Stopped
                }
            },
            (DaemonProbe::Responding, Some(status)) => match status.health.overall_state() {
                HealthState::Healthy => DaemonHealth::Running,
                HealthState::Starting => DaemonHealth::Starting,
                HealthState::Degraded => DaemonHealth::Degraded,
                HealthState::Unhealthy => DaemonHealth::Failed,
            },
            (DaemonProbe::Responding, None) => DaemonHealth::Unreachable,
        }
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.status
            .as_ref()
            .map(|status| status.pid)
            .or_else(|| self.runtime.as_ref().map(|record| record.pid))
    }

    #[cfg(test)]
    pub(crate) fn test(state: DaemonHealth) -> Self {
        let health = match state {
            DaemonHealth::Running | DaemonHealth::Stopped | DaemonHealth::Unreachable => {
                HealthState::Healthy
            },
            DaemonHealth::Starting => HealthState::Starting,
            DaemonHealth::Degraded => HealthState::Degraded,
            DaemonHealth::Failed => HealthState::Unhealthy,
        };
        let probe = match state {
            DaemonHealth::Stopped => DaemonProbe::Stopped,
            DaemonHealth::Unreachable => DaemonProbe::Unreachable {
                message: "unreachable".to_owned(),
            },
            DaemonHealth::Running
            | DaemonHealth::Starting
            | DaemonHealth::Degraded
            | DaemonHealth::Failed => DaemonProbe::Responding,
        };
        let status = match state {
            DaemonHealth::Stopped | DaemonHealth::Unreachable => None,
            _ => Some(DaemonStatus {
                version: "test".to_owned(),
                pid: 1,
                instance_id: "test-instance".to_owned(),
                executable: "/bin/omnifs".into(),
                config_dir: "/tmp/omnifs".into(),
                cache_dir: "/tmp/omnifs/cache".into(),
                attach_tcp: None,
                filesystems: Vec::new(),
                mounts: Vec::new(),
                offline: false,
                health: Box::new(omnifs_api::DaemonHealth::new(
                    omnifs_api::HealthReport::new(health, "test"),
                    omnifs_api::HealthReport::new(HealthState::Healthy, "test"),
                    omnifs_api::HealthReport::new(HealthState::Healthy, "test"),
                )),
            }),
        };
        let runtime = (state == DaemonHealth::Unreachable).then(|| {
            DaemonRecord::new(
                Revision::new("a".repeat(40)).expect("test revision"),
                "/tmp/omnifs/control.sock".into(),
                1,
                "test-instance".to_owned(),
                false,
            )
        });
        Self {
            status,
            probe,
            runtime,
        }
    }
}

impl From<Result<Option<DaemonStatus>, anyhow::Error>> for DaemonFacts {
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

/// Ordered by declared variant, least to most severe: derived `Ord` is the
/// precedence a status-row headline picks from (`status.rs::mount_row_state`),
/// never a "most severe of several fields" tie-break on its own (a merely
/// informational `Neutral`, such as auth `not needed`, must not outrank a
/// genuinely live serving state just because it sorts alongside a real
/// `Attention`/`Error` elsewhere on the same row).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    Positive,
    Neutral,
    Attention,
    Error,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct FilesystemStatus {
    #[serde(flatten)]
    pub(crate) spec: fs::Spec,
    pub(crate) state: FilesystemState,
    pub(crate) mount_count: usize,
    pub(crate) fix: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FilesystemState {
    Attached,
    Detached,
    Unknown,
    Failed,
}

impl FilesystemState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Detached => "detached",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
        }
    }

    pub(crate) const fn severity(self) -> Severity {
        match self {
            Self::Attached => Severity::Positive,
            Self::Detached => Severity::Neutral,
            Self::Unknown => Severity::Attention,
            Self::Failed => Severity::Error,
        }
    }

    pub(crate) const fn fix(self) -> Option<&'static str> {
        match self {
            Self::Failed => Some("omnifs logs"),
            Self::Attached | Self::Detached | Self::Unknown => None,
        }
    }

    /// Whether a filesystem in this state counts as a live access surface
    /// The one owner of that predicate: the access lines,
    /// `status`'s filesystem count, and each mount's `access_count` all filter
    /// through it.
    pub(crate) const fn provides_access(self) -> bool {
        matches!(self, Self::Attached)
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
    NotRequired,
    Missing,
    Corrupt { message: String },
}

impl ProviderPinState {
    pub(crate) const fn severity(&self) -> Severity {
        match self {
            Self::Available => Severity::Positive,
            Self::NotRequired => Severity::Neutral,
            Self::Missing => Severity::Attention,
            Self::Corrupt { .. } => Severity::Error,
        }
    }

    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::NotRequired => "not required",
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
    Stopped,
    Failed { message: String },
    NotLoaded,
}

impl ServingState {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Offline => "offline",
            Self::Stopped => "stopped",
            Self::Failed { .. } => "failed",
            Self::NotLoaded => "not loaded",
        }
    }

    pub(crate) const fn severity(&self) -> Severity {
        match self {
            Self::Live => Severity::Positive,
            Self::Offline | Self::Stopped => Severity::Neutral,
            Self::Failed { .. } | Self::NotLoaded => Severity::Error,
        }
    }

    pub(crate) const fn fix(&self) -> Option<&'static str> {
        match self {
            Self::Failed { .. } => Some("omnifs logs"),
            Self::Live | Self::Offline | Self::Stopped | Self::NotLoaded => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct AccessPath {
    pub(crate) protocol: fs::Protocol,
    pub(crate) runtime: fs::Runtime,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionTarget {
    Workspace,
    Mount(String),
    Filesystem(fs::Id),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NextAction {
    Doctor { target: ActionTarget },
    Reauthenticate { mount: String },
    StartDaemon,
    AttachFilesystem { id: fs::Id },
    CreateFilesystem,
    Browse { path: PathBuf },
    EnterFilesystem { id: fs::Id },
}

impl Inventory {
    pub(crate) async fn collect(workspace: &Workspace) -> Result<Self> {
        let repository = workspace.desired_state().observe_repository()?;
        let registry = repository.registry();
        let mount_revision = repository.head_revision()?;
        let applied_revision = repository.applied()?;
        let client = crate::client::DaemonClient::for_workspace(workspace);
        // Capture process identity before a refused control connection can
        // clean the stale record. Doctor must distinguish an unexpected dead
        // daemon from a cleanly stopped workspace.
        let runtime = client.record().ok().flatten();
        let daemon_probe = client.status_optional_checked().await;
        let daemon_status = daemon_probe.as_ref().ok().and_then(Option::as_ref);
        let mut mounts = if let Some(status) = daemon_status.filter(|status| status.offline) {
            offline_mount_statuses(registry, status)
        } else {
            let catalog = workspace.catalog();
            online_mount_statuses(registry, catalog, workspace.credentials(), daemon_status)
        };
        let mount_count = mounts.len();
        let configured_filesystems = workspace.filesystems().list()?;
        let filesystems = filesystem_statuses(
            &configured_filesystems,
            daemon_status,
            daemon_probe.is_ok(),
            mount_count,
        );
        let warmup = crate::provider_warmup::ProviderWarmup::new(
            workspace.warmup().clone(),
            workspace.catalog().clone(),
        )
        .status();
        let access_count = filesystems
            .iter()
            .filter(|filesystem| filesystem.state.provides_access())
            .count();
        for mount in &mut mounts {
            mount.access_count = access_count;
        }
        let mut daemon = DaemonFacts::from(daemon_probe);
        daemon.runtime = runtime;
        Ok(Self {
            home: workspace.identity().output_home(),
            mount_revision,
            applied_revision,
            daemon,
            filesystems,
            mounts,
            warmup,
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
        self.filesystems
            .iter()
            .map(|filesystem| {
                let path = filesystem.spec.location().join(
                    mount_status
                        .root
                        .strip_prefix("/")
                        .unwrap_or(&mount_status.root),
                );
                let state = match filesystem.state {
                    FilesystemState::Attached => match mount_status.serving {
                        ServingState::Live => AccessState::Available,
                        ServingState::Failed { .. } => AccessState::Failed,
                        ServingState::Offline | ServingState::Stopped | ServingState::NotLoaded => {
                            AccessState::Offline
                        },
                    },
                    FilesystemState::Detached | FilesystemState::Unknown => AccessState::Offline,
                    FilesystemState::Failed => AccessState::Failed,
                };
                AccessPath {
                    protocol: filesystem.spec.protocol(),
                    runtime: filesystem.spec.runtime(),
                    path,
                    state,
                }
            })
            .collect()
    }

    pub(crate) fn verdict(&self) -> Verdict {
        let degraded = self.filesystems.iter().any(|entry| {
            entry.state.severity() >= Severity::Attention
                && matches!(
                    self.daemon.health(),
                    DaemonHealth::Running | DaemonHealth::Starting | DaemonHealth::Degraded
                )
        }) || self.mounts.iter().any(|entry| {
            entry.fix.is_some()
                || entry.provider.state.severity() >= Severity::Attention
                || entry.auth.severity() >= Severity::Attention
                || entry.serving.severity() >= Severity::Attention
        }) || matches!(
            self.daemon.health(),
            DaemonHealth::Failed | DaemonHealth::Unreachable
        );
        if degraded {
            Verdict::Degraded
        } else {
            Verdict::Ok
        }
    }

    pub(crate) fn daemon_health(&self) -> DaemonHealth {
        self.daemon.health()
    }

    pub(crate) fn next_action(&self) -> Option<NextAction> {
        if matches!(
            self.daemon_health(),
            DaemonHealth::Degraded | DaemonHealth::Failed | DaemonHealth::Unreachable
        ) {
            return Some(NextAction::Doctor {
                target: ActionTarget::Workspace,
            });
        }
        if let Some(mount) = self.mounts.iter().find(|mount| {
            mount.provider.state.severity() >= Severity::Attention
                || matches!(mount.serving, ServingState::Failed { .. })
                || mount.fix.as_deref() == Some("omnifs doctor")
        }) {
            return Some(NextAction::Doctor {
                target: ActionTarget::Mount(mount.name.clone()),
            });
        }
        if let Some(filesystem) = self.filesystems.iter().find(|filesystem| {
            matches!(
                filesystem.state,
                FilesystemState::Unknown | FilesystemState::Failed
            )
        }) {
            return Some(NextAction::Doctor {
                target: ActionTarget::Filesystem(filesystem.spec.id().clone()),
            });
        }
        if let Some(mount) = self
            .mounts
            .iter()
            .find(|mount| mount.auth.command().is_some())
        {
            return Some(NextAction::Reauthenticate {
                mount: mount.name.clone(),
            });
        }
        if !self.mounts.is_empty()
            && (self.daemon_health() == DaemonHealth::Stopped
                || self.mounts.iter().any(|mount| {
                    matches!(
                        mount.serving,
                        ServingState::Stopped | ServingState::NotLoaded
                    )
                }))
        {
            return Some(NextAction::StartDaemon);
        }
        if let Some(filesystem) = self
            .filesystems
            .iter()
            .find(|filesystem| filesystem.state == FilesystemState::Detached)
        {
            return Some(NextAction::AttachFilesystem {
                id: filesystem.spec.id().clone(),
            });
        }
        if self.filesystems.is_empty() && !self.mounts.is_empty() {
            return Some(NextAction::CreateFilesystem);
        }
        let first_mount = self
            .mounts
            .iter()
            .find(|mount| matches!(mount.serving, ServingState::Live | ServingState::Offline))
            .or_else(|| self.mounts.first());
        if let Some(filesystem) = self.filesystems.iter().find(|filesystem| {
            filesystem.state.provides_access() && filesystem.spec.runtime() == fs::Runtime::Host
        }) {
            let path = first_mount.map_or_else(
                || filesystem.spec.location().to_path_buf(),
                |mount| {
                    filesystem
                        .spec
                        .location()
                        .join(mount.root.strip_prefix("/").unwrap_or(mount.root.as_path()))
                },
            );
            return Some(NextAction::Browse { path });
        }
        self.filesystems
            .iter()
            .find(|filesystem| {
                filesystem.state.provides_access() && filesystem.spec.runtime() != fs::Runtime::Host
            })
            .map(|filesystem| NextAction::EnterFilesystem {
                id: filesystem.spec.id().clone(),
            })
    }

    #[cfg(test)]
    pub(crate) fn test(
        state: DaemonHealth,
        filesystems: Vec<FilesystemStatus>,
        mounts: Vec<MountStatus>,
    ) -> Self {
        Self {
            home: PathBuf::from("/tmp/omnifs"),
            mount_revision: None,
            applied_revision: None,
            daemon: DaemonFacts::test(state),
            filesystems,
            mounts,
            warmup: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Verdict {
    Ok,
    Degraded,
}

/// Build canonical filesystem rows from the daemon's live attachments.
pub(crate) fn filesystem_statuses(
    configured: &[fs::Spec],
    daemon: Option<&DaemonStatus>,
    daemon_known: bool,
    mount_count: usize,
) -> Vec<FilesystemStatus> {
    let mut rows = configured
        .iter()
        .map(|spec| {
            let observed = daemon.and_then(|status| {
                status
                    .filesystems
                    .iter()
                    .find(|observed| observed.id() == spec.id())
            });
            let attached = observed == Some(spec);
            let identity_conflict = observed.is_some() && !attached;
            let state = if !daemon_known {
                FilesystemState::Unknown
            } else if identity_conflict
                || attached
                    && daemon.is_some_and(|status| {
                        status.health.overall_state() == HealthState::Unhealthy
                    })
            {
                FilesystemState::Failed
            } else if attached {
                FilesystemState::Attached
            } else {
                FilesystemState::Detached
            };
            FilesystemStatus {
                spec: spec.clone(),
                state,
                mount_count,
                fix: if identity_conflict {
                    Some("omnifs doctor".to_owned())
                } else {
                    state.fix().map(str::to_owned)
                },
            }
        })
        .collect::<Vec<_>>();

    if let Some(daemon) = daemon {
        rows.extend(
            daemon
                .filesystems
                .iter()
                .filter(|observed| !configured.iter().any(|spec| spec.id() == observed.id()))
                .map(|observed| FilesystemStatus {
                    spec: observed.clone(),
                    state: FilesystemState::Failed,
                    mount_count,
                    fix: Some("omnifs doctor".to_owned()),
                }),
        );
    }

    rows.sort_by(filesystem_cmp);
    rows
}

fn online_mount_statuses(
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
            let serving = derive_serving_state(MountFacts {
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

fn offline_mount_rows(registry: &Registry, loaded: &BTreeSet<&str>) -> Vec<MountStatus> {
    registry
        .iter()
        .map(|(name, spec)| {
            let name = name.to_string();
            let serving = if loaded.contains(name.as_str()) {
                ServingState::Offline
            } else {
                ServingState::NotLoaded
            };
            MountStatus {
                name: name.clone(),
                root: PathBuf::from(format!("/{name}")),
                provider: ProviderPin {
                    name: spec.provider.meta.name.to_string(),
                    version: spec.provider.meta.version.as_ref().map(ToString::to_string),
                    artifact: spec.provider.id.to_string(),
                    state: ProviderPinState::NotRequired,
                },
                auth: AuthState::NotNeeded,
                serving,
                access_count: 0,
                fix: None,
            }
        })
        .collect()
}

fn offline_mount_statuses(registry: &Registry, status: &DaemonStatus) -> Vec<MountStatus> {
    let loaded = status
        .mounts
        .iter()
        .map(|mount| mount.mount.as_str())
        .collect::<BTreeSet<_>>();
    let desired = registry
        .iter()
        .map(|(name, _)| name.to_string())
        .collect::<BTreeSet<_>>();
    let mut rows = offline_mount_rows(registry, &loaded);
    rows.extend(observed_mount_rows(status, &desired));
    rows.extend(invalid_mount_rows(registry));
    rows.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| left.name.cmp(&right.name))
    });
    rows
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
                    state: if status.offline {
                        ProviderPinState::NotRequired
                    } else {
                        ProviderPinState::Available
                    },
                },
                auth,
                serving: if status.offline {
                    ServingState::Offline
                } else {
                    ServingState::Live
                },
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
struct MountFacts {
    provider: Presence,
    daemon: Presence,
    loaded: Presence,
    health: Health,
}

fn derive_serving_state(facts: MountFacts) -> ServingState {
    if matches!(facts.daemon, Presence::Absent) {
        return ServingState::Stopped;
    }
    if matches!(facts.provider, Presence::Absent) {
        return ServingState::NotLoaded;
    }
    if matches!(facts.health, Health::Unhealthy) {
        return ServingState::Failed {
            message: "daemon health is unhealthy".into(),
        };
    }
    if matches!(facts.loaded, Presence::Present) {
        ServingState::Live
    } else {
        ServingState::NotLoaded
    }
}

fn filesystem_cmp(left: &FilesystemStatus, right: &FilesystemStatus) -> Ordering {
    left.spec
        .runtime()
        .cmp(&right.spec.runtime())
        .then_with(|| {
            left.spec
                .protocol()
                .as_str()
                .cmp(right.spec.protocol().as_str())
        })
        .then_with(|| left.spec.location().cmp(right.spec.location()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut daemon = DaemonFacts::test(DaemonHealth::Running);
        daemon.status.as_mut().unwrap().mounts = vec![
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
        let daemon = daemon.status.as_ref();

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
            serving: ServingState::Stopped,
            access_count: 0,
            fix: auth.command().map(ToOwned::to_owned),
        };
        let inventory = Inventory::test(DaemonHealth::Stopped, vec![], vec![mount]);
        assert_eq!(inventory.verdict(), Verdict::Degraded);
        assert_eq!(
            inventory.mounts[0].auth.command(),
            Some("omnifs mount reauth x")
        );
    }

    #[test]
    fn access_paths_are_derived_on_request() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![FilesystemStatus {
                spec: fs::Spec::new(
                    "test".parse().unwrap(),
                    fs::Protocol::Fuse,
                    fs::Runtime::Host,
                    "/mnt".into(),
                )
                .unwrap(),
                state: FilesystemState::Attached,
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
            derive_serving_state(MountFacts {
                provider: Presence::Absent,
                daemon: Presence::Present,
                loaded: Presence::Absent,
                health: Health::Unhealthy,
            }),
            ServingState::NotLoaded,
            "missing artifact outranks daemon failure"
        );
        assert_eq!(
            derive_serving_state(MountFacts {
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
            derive_serving_state(MountFacts {
                provider: Presence::Present,
                daemon: Presence::Present,
                loaded: Presence::Absent,
                health: Health::Healthy,
            }),
            ServingState::NotLoaded,
            "a reachable daemon does not imply every spec converged"
        );
        assert_eq!(
            derive_serving_state(MountFacts {
                provider: Presence::Present,
                daemon: Presence::Present,
                loaded: Presence::Present,
                health: Health::Healthy,
            }),
            ServingState::Live
        );
        assert_eq!(
            derive_serving_state(MountFacts {
                provider: Presence::Present,
                daemon: Presence::Absent,
                loaded: Presence::Absent,
                health: Health::Healthy,
            }),
            ServingState::Stopped
        );
    }

    #[test]
    fn probe_failure_is_unreachable_only_when_runtime_expected() {
        let probe = Err(anyhow::anyhow!("connection refused"));
        let expected = DaemonRecord::new(
            omnifs_workspace::mounts::Revision::new("a".repeat(40)).unwrap(),
            PathBuf::from("/home/.omnifs/filesystems/runtime/local.sock"),
            42,
            "instance".into(),
            false,
        );
        let mut unreachable = DaemonFacts::from(probe);
        unreachable.runtime = Some(expected);
        assert_eq!(unreachable.health(), DaemonHealth::Unreachable);
        assert_eq!(
            DaemonFacts::from(Err(anyhow::anyhow!("connection refused"))).health(),
            DaemonHealth::Stopped
        );
    }

    #[test]
    fn daemon_health_maps_to_distinct_operational_states() {
        for (health, expected) in [
            (HealthState::Healthy, DaemonHealth::Running),
            (HealthState::Starting, DaemonHealth::Starting),
            (HealthState::Degraded, DaemonHealth::Degraded),
            (HealthState::Unhealthy, DaemonHealth::Failed),
        ] {
            let status = DaemonStatus {
                version: "test".into(),
                pid: 1,
                instance_id: "instance".into(),
                executable: "/bin/omnifs".into(),
                config_dir: "/home/.omnifs".into(),
                cache_dir: "/home/.omnifs/cache".into(),
                attach_tcp: None,
                filesystems: Vec::new(),
                mounts: Vec::new(),
                offline: false,
                health: Box::new(omnifs_api::DaemonHealth::new(
                    omnifs_api::HealthReport::new(health, "test"),
                    omnifs_api::HealthReport::new(HealthState::Healthy, "test"),
                    omnifs_api::HealthReport::new(HealthState::Healthy, "test"),
                )),
            };
            assert_eq!(DaemonFacts::from(Ok(Some(status))).health(), expected);
        }
    }

    #[test]
    fn access_paths_cover_every_filesystem_and_mount_state() {
        let inventory = Inventory::test(
            DaemonHealth::Running,
            vec![
                FilesystemStatus {
                    spec: fs::Spec::new(
                        "host".parse().unwrap(),
                        fs::Protocol::Fuse,
                        fs::Runtime::Host,
                        "/host".into(),
                    )
                    .unwrap(),
                    state: FilesystemState::Attached,
                    mount_count: 1,
                    fix: None,
                },
                FilesystemStatus {
                    spec: fs::Spec::new(
                        "docker".parse().unwrap(),
                        fs::Protocol::Fuse,
                        fs::Runtime::Docker,
                        "/omnifs".into(),
                    )
                    .unwrap(),
                    state: FilesystemState::Attached,
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
    fn daemon_down_inventory_has_no_filesystem_rows() {
        let rows = filesystem_statuses(&[], None, true, 1);
        assert!(rows.is_empty());
    }

    #[test]
    fn daemon_down_inventory_retains_configured_detached_filesystems() {
        let spec = fs::Spec::new(
            fs::Id::new("local").unwrap(),
            fs::Protocol::Nfs,
            fs::Runtime::Host,
            PathBuf::from("/mnt/local"),
        )
        .unwrap();
        let rows = filesystem_statuses(&[spec], None, true, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].spec.id().as_str(), "local");
        assert_eq!(rows[0].state, FilesystemState::Detached);
    }

    #[test]
    fn configured_identity_conflict_is_failed_not_hidden_as_detached() {
        let spec = fs::Spec::new(
            fs::Id::new("local").unwrap(),
            fs::Protocol::Nfs,
            fs::Runtime::Host,
            PathBuf::from("/mnt/local"),
        )
        .unwrap();
        let mut daemon = DaemonFacts::test(DaemonHealth::Running).status.unwrap();
        daemon.filesystems.push(
            fs::Spec::new(
                spec.id().clone(),
                fs::Protocol::Fuse,
                fs::Runtime::Docker,
                PathBuf::from(fs::GUEST_LOCATION),
            )
            .unwrap(),
        );

        let rows = filesystem_statuses(&[spec], Some(&daemon), true, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, FilesystemState::Failed);
        assert_eq!(rows[0].fix.as_deref(), Some("omnifs doctor"));
    }

    #[test]
    fn verdict_matrix_maps_actionable_states() {
        let base = Inventory::test(
            DaemonHealth::Stopped,
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
                serving: ServingState::Stopped,
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
        unmanaged.daemon = DaemonFacts::test(DaemonHealth::Running);
        unmanaged.filesystems.push(FilesystemStatus {
            spec: fs::Spec::new(
                "test".parse().unwrap(),
                fs::Protocol::Fuse,
                fs::Runtime::Host,
                "/mnt".into(),
            )
            .unwrap(),
            state: FilesystemState::Failed,
            mount_count: 1,
            fix: Some("omnifs fs detach --name test".into()),
        });
        assert_eq!(unmanaged.verdict(), Verdict::Degraded);
        let mut unreachable = base;
        unreachable.daemon = DaemonFacts::test(DaemonHealth::Unreachable);
        assert_eq!(unreachable.verdict(), Verdict::Degraded);
    }

    #[test]
    fn next_action_has_one_stable_priority_order() {
        let mount = MountStatus {
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
        };
        let filesystem = FilesystemStatus {
            spec: fs::Spec::new(
                "host".parse().unwrap(),
                fs::Protocol::Nfs,
                fs::Runtime::Host,
                "/mnt/omnifs".into(),
            )
            .unwrap(),
            state: FilesystemState::Attached,
            mount_count: 1,
            fix: None,
        };

        let healthy = Inventory::test(
            DaemonHealth::Running,
            vec![filesystem.clone()],
            vec![mount.clone()],
        );
        assert_eq!(
            healthy.next_action(),
            Some(NextAction::Browse {
                path: "/mnt/omnifs/github".into()
            })
        );

        let mut detached = healthy.clone();
        detached.filesystems[0].state = FilesystemState::Detached;
        assert_eq!(
            detached.next_action(),
            Some(NextAction::AttachFilesystem {
                id: "host".parse().unwrap()
            })
        );

        let no_filesystem = Inventory::test(DaemonHealth::Running, Vec::new(), vec![mount.clone()]);
        assert_eq!(
            no_filesystem.next_action(),
            Some(NextAction::CreateFilesystem)
        );

        let mut auth = healthy.clone();
        auth.mounts[0].auth = AuthState::Expired {
            command: "omnifs mount reauth github".into(),
        };
        assert_eq!(
            auth.next_action(),
            Some(NextAction::Reauthenticate {
                mount: "github".into()
            })
        );

        let mut broken = auth;
        broken.filesystems[0].state = FilesystemState::Failed;
        assert_eq!(
            broken.next_action(),
            Some(NextAction::Doctor {
                target: ActionTarget::Filesystem("host".parse().unwrap())
            })
        );

        broken.daemon = DaemonFacts::test(DaemonHealth::Unreachable);
        assert_eq!(
            broken.next_action(),
            Some(NextAction::Doctor {
                target: ActionTarget::Workspace
            })
        );
    }

    #[test]
    fn structured_inventory_keeps_runtime_expectation_and_absolute_identity() {
        let inventory = Inventory::test(
            DaemonHealth::Stopped,
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
                serving: ServingState::Stopped,
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
