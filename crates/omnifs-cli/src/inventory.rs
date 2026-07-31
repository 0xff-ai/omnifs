//! Authoritative application inventory.
//!
//! This module owns the typed facts consumed by status, list, and receipt
//! surfaces. Collection is deliberately at the edge; all joins, sorting, and
//! verdict decisions below are pure.

use anyhow::Result;
#[cfg(test)]
use omnifs_api::DaemonPhase;
use omnifs_api::{CredentialHealth, DaemonInventory, HealthState, MountHealth, MountRecord};
use omnifs_core::MountRevision;
#[cfg(test)]
use omnifs_core::{ATTACHMENT_GUEST_LOCATION, AttachmentProtocol};
use omnifs_core::{AttachmentRuntime, AttachmentSpec, ResourceName};
use serde::Serialize;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::ui::output::ResultVerdict;
use omnifs_bootstrap::Profile;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Inventory {
    pub(crate) home: PathBuf,
    pub(crate) durable_revision: Option<MountRevision>,
    pub(crate) serving_revision: Option<MountRevision>,
    pub(crate) daemon: DaemonFacts,
    pub(crate) filesystems: Vec<FilesystemStatus>,
    pub(crate) mounts: Vec<MountStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DaemonFacts {
    pub(crate) status: Option<DaemonInventory>,
    pub(crate) probe: DaemonProbe,
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

impl DaemonHealth {
    /// The one (severity, label) pair every consumer renders a bare daemon
    /// health as: `omnifs status`'s context strip and `omnifs doctor`'s
    /// daemon row both start from this instead of each spelling out its own
    /// six-variant match. `Running` renders plainly as "running" here;
    /// status's context strip additionally folds in the whole profile's
    /// verdict (mounts and filesystems, not just the daemon) to show
    /// "healthy" instead when everything else is clean too, a distinction
    /// doctor's daemon-only row deliberately does not make (mount and
    /// filesystem problems get their own rows there).
    pub(crate) const fn descriptor(self) -> (Severity, &'static str) {
        match self {
            Self::Running => (Severity::Positive, "running"),
            Self::Starting => (Severity::Attention, "starting"),
            Self::Degraded => (Severity::Attention, "degraded"),
            Self::Stopped => (Severity::Neutral, "stopped"),
            Self::Failed => (Severity::Failure, "failed"),
            Self::Unreachable => (Severity::Failure, "unreachable"),
        }
    }
}

impl DaemonFacts {
    pub(crate) fn health(&self) -> DaemonHealth {
        match (&self.probe, self.status.as_ref()) {
            (DaemonProbe::Stopped, _) => DaemonHealth::Stopped,
            (DaemonProbe::Unreachable { .. }, _) | (DaemonProbe::Responding, None) => {
                DaemonHealth::Unreachable
            },
            (DaemonProbe::Responding, Some(status)) => match status.health.overall_state() {
                HealthState::Healthy => DaemonHealth::Running,
                HealthState::Starting => DaemonHealth::Starting,
                HealthState::Degraded => DaemonHealth::Degraded,
                HealthState::Unhealthy => DaemonHealth::Failed,
            },
        }
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.status.as_ref().map(|status| status.info.pid)
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
            _ => Some(DaemonInventory {
                info: omnifs_api::DaemonInfo {
                    version: "test".to_owned(),
                    pid: 1,
                    instance_id: "test-instance".to_owned(),
                    executable: "/bin/omnifs".into(),
                    attach_unix: None,
                    attach_tcp: None,
                },
                phase: DaemonPhase::Ready,
                durable_revision: None,
                serving_revision: None,
                health: omnifs_api::DaemonHealth::new(
                    omnifs_api::HealthReport::new(health, "test"),
                    omnifs_api::HealthReport::new(HealthState::Healthy, "test"),
                    omnifs_api::HealthReport::new(HealthState::Healthy, "test"),
                ),
                mounts: Vec::new(),
                credentials: Vec::new(),
                attachments: Vec::new(),
            }),
        };
        Self { status, probe }
    }
}

impl From<Result<Option<DaemonInventory>, anyhow::Error>> for DaemonFacts {
    fn from(probe: Result<Option<DaemonInventory>, anyhow::Error>) -> Self {
        match probe {
            Ok(Some(status)) => Self {
                status: Some(status),
                probe: DaemonProbe::Responding,
            },
            Ok(None) => Self {
                status: None,
                probe: DaemonProbe::Stopped,
            },
            Err(error) => Self {
                status: None,
                probe: DaemonProbe::Unreachable {
                    message: format!("{error:#}"),
                },
            },
        }
    }
}

/// Ordered by declared variant, least to most severe: derived `Ord` is the
/// precedence a status-row headline picks from (`status.rs::mount_row_state`),
/// never a "most severe of several fields" tie-break on its own (a merely
/// informational `Neutral`, such as auth `not needed`, must not outrank a
/// genuinely live serving state just because it sorts alongside a real
/// `Attention`/`Failure` elsewhere on the same row).
///
/// The exact same 4-variant shape as [`crate::ui::table::Severity`], and
/// deliberately kept as a separate type: that one is presentation-only and
/// holds no domain knowledge (see its own doc). [`From`] bridges the two at
/// `status.rs`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    Positive,
    Neutral,
    Attention,
    Failure,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct FilesystemStatus {
    pub(crate) name: ResourceName,
    #[serde(flatten)]
    pub(crate) spec: AttachmentSpec,
    pub(crate) state: FilesystemState,
    pub(crate) mount_count: usize,
    pub(crate) fix: Option<String>,
}

impl PartialOrd for FilesystemStatus {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FilesystemStatus {
    fn cmp(&self, other: &Self) -> Ordering {
        self.spec
            .runtime()
            .cmp(&other.spec.runtime())
            .then_with(|| {
                self.spec
                    .protocol()
                    .as_str()
                    .cmp(other.spec.protocol().as_str())
            })
            .then_with(|| self.spec.location().cmp(other.spec.location()))
    }
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
            Self::Failed => Severity::Failure,
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
}

impl MountStatus {
    /// Which subsystem should headline this mount's row, in fixed
    /// precedence: a provider pin problem outranks an auth problem, which
    /// outranks the serving state itself. Never a "most severe of three"
    /// tie-break (see `Severity`'s own doc): a merely informational
    /// `Neutral` (auth `not needed`) must not outrank a genuinely live
    /// serving state just because it sorts alongside a real `Attention` or
    /// `Failure` elsewhere on the same row.
    pub(crate) fn headline(&self) -> (Severity, &'static str) {
        if self.provider.state.severity() >= Severity::Attention {
            return (self.provider.state.severity(), self.provider.state.label());
        }
        if self.auth.severity() >= Severity::Attention {
            return (self.auth.severity(), self.auth.label());
        }
        (self.serving.severity(), self.serving.label())
    }

    /// Whether any subsystem (provider pin, auth, or serving) needs
    /// attention. Equivalent to `headline().0 >= Severity::Attention`
    /// because the precedence chain always surfaces a real `Attention` or
    /// worse severity whenever any of the three carries one.
    pub(crate) fn needs_attention(&self) -> bool {
        self.headline().0 >= Severity::Attention
    }

    /// Whether this mount's own state is the daemon's next actionable
    /// Doctor target: a provider pin problem, or a serving state that has
    /// actually failed. Auth is deliberately excluded here: an auth problem
    /// gets its own `Reauthenticate` action further down
    /// `Inventory::next_action`'s priority order, so folding it into this
    /// predicate would make both actions fire for the same mount.
    pub(crate) fn needs_doctor(&self) -> bool {
        self.provider.state.severity() >= Severity::Attention
            || matches!(self.serving, ServingState::Failed { .. })
    }

    /// The host filesystem path this mount's files are reachable at, or
    /// empty when the mount isn't live or no host filesystem is attached.
    pub(crate) fn access_path(&self, host_location: Option<&Path>) -> String {
        if self.serving != ServingState::Live {
            return String::new();
        }
        host_location.map_or_else(String::new, |location| {
            location
                .join(self.root.strip_prefix("/").unwrap_or(self.root.as_path()))
                .display()
                .to_string()
        })
    }

    /// One mount's status derived from its daemon record. The one owner of
    /// this record-to-status projection: `mount ls`'s table (via
    /// `all_observed`) and `mount show`'s detail card both read a mount's
    /// health, auth, and serving state through this same conversion instead
    /// of each deriving its own labels from the raw `MountHealth`.
    pub(crate) fn from_record(mount: &MountRecord) -> Self {
        let name = mount.definition.name.to_string();
        let (provider_state, serving) = match &mount.health {
            MountHealth::Active | MountHealth::AuthRequired => {
                (ProviderPinState::Available, ServingState::Live)
            },
            MountHealth::ProviderUnavailable { reason } => (
                ProviderPinState::Missing,
                ServingState::Failed {
                    message: reason.clone(),
                },
            ),
            MountHealth::Failed { reason } => (
                ProviderPinState::Corrupt {
                    message: reason.clone(),
                },
                ServingState::Failed {
                    message: reason.clone(),
                },
            ),
        };
        let auth = AuthState::from_mount(mount, &name);
        Self {
            root: PathBuf::from(format!("/{name}")),
            provider: ProviderPin {
                name: mount.provider.name.clone(),
                version: mount.provider.version.clone(),
                artifact: mount.provider.id.to_string(),
                state: provider_state,
            },
            name,
            auth,
            serving,
            access_count: 0,
        }
    }

    /// Build canonical mount rows from the daemon's reported inventory,
    /// sorted by root then name. Not a `From` impl: the sort is part of
    /// this constructor's contract, and a `From` would hide it.
    fn all_observed(inventory: &DaemonInventory) -> Vec<Self> {
        let mut mounts = inventory
            .mounts
            .iter()
            .map(Self::from_record)
            .collect::<Vec<_>>();
        mounts.sort_by(|left, right| {
            left.root
                .cmp(&right.root)
                .then_with(|| left.name.cmp(&right.name))
        });
        mounts
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProviderPin {
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    pub(crate) artifact: String,
    pub(crate) state: ProviderPinState,
}

impl std::fmt::Display for ProviderPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let identity = self.version.as_ref().map_or_else(
            || self.name.clone(),
            |version| format!("{}@{version}", self.name),
        );
        if self.state.severity() >= Severity::Attention {
            write!(f, "{identity} ({})", self.state.label())
        } else {
            write!(f, "{identity}")
        }
    }
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
            Self::Corrupt { .. } => Severity::Failure,
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
    pub(crate) const fn severity(&self) -> Severity {
        match self {
            Self::NotNeeded => Severity::Neutral,
            Self::Ready => Severity::Positive,
            Self::Missing { .. } | Self::Expired { .. } => Severity::Attention,
            Self::Error { .. } => Severity::Failure,
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

    fn from_mount(mount: &omnifs_api::MountRecord, name: &str) -> Self {
        if mount.definition.auth.is_none() {
            return Self::NotNeeded;
        }
        let command = || format!("omnifs mount reauth {name}");
        match mount.auth_health {
            Some(CredentialHealth::Missing) | None if mount.health == MountHealth::AuthRequired => {
                Self::Missing { command: command() }
            },
            Some(CredentialHealth::Expired) => Self::Expired { command: command() },
            Some(CredentialHealth::RefreshFailed) => Self::Error {
                message: "credential refresh failed".to_owned(),
                command: command(),
            },
            Some(CredentialHealth::NeedsConsent) => Self::Error {
                message: "credential needs consent".to_owned(),
                command: command(),
            },
            Some(
                CredentialHealth::Ready
                | CredentialHealth::ExpiringSoon
                | CredentialHealth::StaticUnvalidated
                | CredentialHealth::Missing,
            )
            | None => Self::Ready,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ServingState {
    Live,
    Stopped,
    Failed { message: String },
}

impl ServingState {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Stopped => "stopped",
            Self::Failed { .. } => "failed",
        }
    }

    pub(crate) const fn severity(&self) -> Severity {
        match self {
            Self::Live => Severity::Positive,
            Self::Stopped => Severity::Neutral,
            Self::Failed { .. } => Severity::Failure,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionTarget {
    Profile,
    Mount(String),
    Filesystem(ResourceName),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NextAction {
    Doctor { target: ActionTarget },
    Reauthenticate { mount: String },
    AttachFilesystem { id: ResourceName },
    CreateFilesystem,
    Browse { path: PathBuf },
    EnterFilesystem { id: ResourceName },
}

impl Inventory {
    pub(crate) async fn collect_rpc() -> Result<Self> {
        let endpoint = Profile::resolve()?;
        let rpc = crate::rpc::RpcClient::resolve()?;
        let (daemon, mounts, daemon_status, daemon_known) = match rpc.inventory().await {
            Ok(inventory) => {
                let mounts = MountStatus::all_observed(&inventory);
                let daemon = DaemonFacts {
                    status: Some(inventory.clone()),
                    probe: DaemonProbe::Responding,
                };
                (daemon, mounts, Some(inventory), true)
            },
            Err(error) => {
                let probe = match endpoint.read_process_identity() {
                    Ok(Some(_)) => DaemonProbe::Unreachable {
                        message: format!("{error:#}"),
                    },
                    Ok(None) => DaemonProbe::Stopped,
                    Err(identity_error) => DaemonProbe::Unreachable {
                        message: format!(
                            "{error:#}; cannot read process identity: {identity_error}"
                        ),
                    },
                };
                let daemon_known = probe == DaemonProbe::Stopped;
                (
                    DaemonFacts {
                        status: None,
                        probe,
                    },
                    Vec::new(),
                    None,
                    daemon_known,
                )
            },
        };
        let mount_count = mounts.len();
        let filesystems = daemon_status.as_ref().map_or_else(Vec::new, |status| {
            filesystem_statuses(&status.attachments, Some(status), daemon_known, mount_count)
        });
        let mut inventory = Self {
            home: endpoint.root().to_path_buf(),
            durable_revision: daemon_status
                .as_ref()
                .and_then(|status| status.durable_revision),
            serving_revision: daemon_status
                .as_ref()
                .and_then(|status| status.serving_revision),
            daemon,
            filesystems,
            mounts,
        };
        let access_count = inventory.attached_filesystem_count();
        for mount in &mut inventory.mounts {
            mount.access_count = access_count;
        }
        Ok(inventory)
    }

    /// Filesystem rows that provide live access, in inventory order.
    pub(crate) fn attached_filesystems(&self) -> impl Iterator<Item = &FilesystemStatus> {
        self.filesystems
            .iter()
            .filter(|filesystem| filesystem.state.provides_access())
    }

    /// Count of filesystem rows that provide live access. The one owner
    /// `collect_rpc` uses to seed every mount's `access_count`, so counting
    /// again from a rendered `Inventory` can never drift from what was
    /// recorded at collection time.
    pub(crate) fn attached_filesystem_count(&self) -> usize {
        self.attached_filesystems().count()
    }

    /// The first attached host filesystem's location, if any.
    pub(crate) fn primary_host_location(&self) -> Option<&Path> {
        self.attached_filesystems()
            .find(|filesystem| filesystem.spec.runtime() == AttachmentRuntime::Host)
            .map(|filesystem| filesystem.spec.location())
    }

    pub(crate) fn verdict(&self) -> ResultVerdict {
        let degraded = self.filesystems.iter().any(|entry| {
            entry.state.severity() >= Severity::Attention
                && matches!(
                    self.daemon.health(),
                    DaemonHealth::Running | DaemonHealth::Starting | DaemonHealth::Degraded
                )
        }) || self.mounts.iter().any(MountStatus::needs_attention)
            || matches!(
                self.daemon.health(),
                DaemonHealth::Failed | DaemonHealth::Unreachable
            );
        if degraded {
            ResultVerdict::Degraded
        } else {
            ResultVerdict::Ok
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
                target: ActionTarget::Profile,
            });
        }
        if let Some(mount) = self.mounts.iter().find(|mount| mount.needs_doctor()) {
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
                target: ActionTarget::Filesystem(filesystem.name.clone()),
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
        if let Some(filesystem) = self
            .filesystems
            .iter()
            .find(|filesystem| filesystem.state == FilesystemState::Detached)
        {
            return Some(NextAction::AttachFilesystem {
                id: filesystem.name.clone(),
            });
        }
        if self.filesystems.is_empty() && !self.mounts.is_empty() {
            return Some(NextAction::CreateFilesystem);
        }
        let first_mount = self
            .mounts
            .iter()
            .find(|mount| mount.serving == ServingState::Live)
            .or_else(|| self.mounts.first());
        if let Some(filesystem) = self.filesystems.iter().find(|filesystem| {
            filesystem.state.provides_access()
                && filesystem.spec.runtime() == AttachmentRuntime::Host
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
                filesystem.state.provides_access()
                    && filesystem.spec.runtime() != AttachmentRuntime::Host
            })
            .map(|filesystem| NextAction::EnterFilesystem {
                id: filesystem.name.clone(),
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
            durable_revision: None,
            serving_revision: None,
            daemon: DaemonFacts::test(state),
            filesystems,
            mounts,
        }
    }
}

/// Build canonical filesystem rows from the daemon's live attachments.
pub(crate) fn filesystem_statuses(
    configured: &[omnifs_api::AttachmentDefinition],
    daemon: Option<&DaemonInventory>,
    daemon_known: bool,
    mount_count: usize,
) -> Vec<FilesystemStatus> {
    let mut rows = configured
        .iter()
        .map(|definition| {
            let spec = &definition.spec;
            let observed = daemon.and_then(|status| {
                status
                    .attachments
                    .iter()
                    .find(|observed| observed.name == definition.name)
            });
            let attached = observed.is_some_and(|observed| observed.spec == *spec);
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
                name: definition.name.clone(),
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
                .attachments
                .iter()
                .filter(|observed| {
                    !configured
                        .iter()
                        .any(|definition| definition.name == observed.name)
                })
                .map(|observed| FilesystemStatus {
                    name: observed.name.clone(),
                    spec: observed.spec.clone(),
                    state: if daemon.health.overall_state() == HealthState::Unhealthy {
                        FilesystemState::Failed
                    } else {
                        FilesystemState::Attached
                    },
                    mount_count,
                    fix: None,
                }),
        );
    }

    rows.sort();
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_state_labels_are_readable_without_changing_wire_names() {
        assert_eq!(AuthState::NotNeeded.severity(), Severity::Neutral);
        assert_eq!(AuthState::NotNeeded.label(), "not needed");

        assert_eq!(
            serde_json::to_value(AuthState::NotNeeded).unwrap()["state"],
            "not_needed"
        );
    }

    #[test]
    fn expired_auth_degrades_verdict_and_keeps_its_reauth_command() {
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
        };
        let inventory = Inventory::test(DaemonHealth::Stopped, vec![], vec![mount]);
        assert_eq!(inventory.verdict(), ResultVerdict::Degraded);
        assert_eq!(
            inventory.mounts[0].auth.command(),
            Some("omnifs mount reauth x")
        );
    }

    #[test]
    fn probe_failure_is_unreachable() {
        assert_eq!(
            DaemonFacts::from(Err(anyhow::anyhow!("connection refused"))).health(),
            DaemonHealth::Unreachable
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
            let status = DaemonInventory {
                info: omnifs_api::DaemonInfo {
                    version: "test".into(),
                    pid: 1,
                    instance_id: "instance".into(),
                    executable: "/bin/omnifs".into(),
                    attach_unix: None,
                    attach_tcp: None,
                },
                phase: DaemonPhase::Ready,
                durable_revision: None,
                serving_revision: None,
                health: omnifs_api::DaemonHealth::new(
                    omnifs_api::HealthReport::new(health, "test"),
                    omnifs_api::HealthReport::new(HealthState::Healthy, "test"),
                    omnifs_api::HealthReport::new(HealthState::Healthy, "test"),
                ),
                mounts: Vec::new(),
                credentials: Vec::new(),
                attachments: Vec::new(),
            };
            assert_eq!(DaemonFacts::from(Ok(Some(status))).health(), expected);
        }
    }

    #[test]
    fn daemon_down_inventory_has_no_filesystem_rows() {
        let rows = filesystem_statuses(&[], None, true, 1);
        assert!(rows.is_empty());
    }

    #[test]
    fn daemon_down_inventory_retains_configured_detached_filesystems() {
        let spec = AttachmentSpec::new(
            AttachmentProtocol::Nfs,
            AttachmentRuntime::Host,
            PathBuf::from("/mnt/local"),
            None,
            None,
        )
        .unwrap();
        let definition = omnifs_api::AttachmentDefinition {
            name: "local".parse().unwrap(),
            spec,
        };
        let rows = filesystem_statuses(&[definition], None, true, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name.as_str(), "local");
        assert_eq!(rows[0].state, FilesystemState::Detached);
    }

    #[test]
    fn configured_identity_conflict_is_failed_not_hidden_as_detached() {
        let spec = AttachmentSpec::new(
            AttachmentProtocol::Nfs,
            AttachmentRuntime::Host,
            PathBuf::from("/mnt/local"),
            None,
            None,
        )
        .unwrap();
        let mut daemon = DaemonFacts::test(DaemonHealth::Running).status.unwrap();
        daemon.attachments.push(omnifs_api::AttachmentDefinition {
            name: "local".parse().unwrap(),
            spec: AttachmentSpec::new(
                AttachmentProtocol::Fuse,
                AttachmentRuntime::Docker,
                PathBuf::from(ATTACHMENT_GUEST_LOCATION),
                Some("image".to_owned()),
                None,
            )
            .unwrap(),
        });

        let rows = filesystem_statuses(
            &[omnifs_api::AttachmentDefinition {
                name: "local".parse().unwrap(),
                spec,
            }],
            Some(&daemon),
            true,
            1,
        );
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
            }],
        );
        assert_eq!(
            base.verdict(),
            ResultVerdict::Ok,
            "deliberately stopped is neutral"
        );
        let mut expired = base.clone();
        expired.mounts[0].auth = AuthState::Expired {
            command: "omnifs mount reauth x".into(),
        };
        assert_eq!(expired.verdict(), ResultVerdict::Degraded);
        let mut unmanaged = base.clone();
        unmanaged.daemon = DaemonFacts::test(DaemonHealth::Running);
        let protocol = if cfg!(target_os = "linux") {
            AttachmentProtocol::Fuse
        } else {
            AttachmentProtocol::Nfs
        };
        unmanaged.filesystems.push(FilesystemStatus {
            name: "test".parse().unwrap(),
            spec: AttachmentSpec::new(
                protocol,
                AttachmentRuntime::Host,
                "/mnt".into(),
                None,
                None,
            )
            .unwrap(),
            state: FilesystemState::Failed,
            mount_count: 1,
            fix: Some("omnifs attachment rm test".into()),
        });
        assert_eq!(unmanaged.verdict(), ResultVerdict::Degraded);
        let mut unreachable = base;
        unreachable.daemon = DaemonFacts::test(DaemonHealth::Unreachable);
        assert_eq!(unreachable.verdict(), ResultVerdict::Degraded);
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
        };
        let filesystem = FilesystemStatus {
            name: "host".parse().unwrap(),
            spec: AttachmentSpec::new(
                AttachmentProtocol::Nfs,
                AttachmentRuntime::Host,
                "/mnt/omnifs".into(),
                None,
                None,
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
                target: ActionTarget::Profile
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
