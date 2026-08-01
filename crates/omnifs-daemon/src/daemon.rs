//! The daemon runtime: namespace endpoints, supervision, and the health
//! rollup the control plane reports.

use super::context::{ATTACH_PORT_COUNT, ATTACH_PORT_MIN, DaemonContext};
use super::provider_bundle::EmbeddedProviders;
use anyhow::Context as _;
use omnifs_api::{
    CONTROL_SHUTDOWN_DRAIN_SECS, ControlError, ControlErrorCode, CredentialHealth, DaemonHealth,
    DaemonInventory, DaemonPhase, DaemonRecovery, DaemonStatus, FilesystemAccess,
    FilesystemCommand, FilesystemDefinition, FilesystemPhase as ApiFilesystemPhase,
    FilesystemStatus, GetFilesystemAccessRequest, HealthReport, HealthState, MountHealth,
    MountInfo, MountRecord,
};
use omnifs_engine::{Inspector, ServingCell};
use omnifs_state::StateStore;
use std::fmt::Write as _;
use std::net::Ipv4Addr;
use std::sync::{Arc, OnceLock};
use tracing::info;

use crate::control::mapping::{
    api_credential_health_kind, api_credential_status, api_mount_record,
};

pub(crate) const DRAIN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(CONTROL_SHUTDOWN_DRAIN_SECS);
/// Loopback, or Linux's Docker bridge gateway when that interface exists.
/// Never binds all interfaces. A `getifaddrs` failure is a genuine "we do
/// not know the answer" and must propagate rather than silently bind
/// loopback, which would publish a ready daemon a Docker guest cannot reach.
#[cfg(target_os = "linux")]
fn attach_bind_ip() -> anyhow::Result<Ipv4Addr> {
    for interface in nix::ifaddrs::getifaddrs().context("enumerate host network interfaces")? {
        if interface.interface_name != "docker0" {
            continue;
        }
        if let Some(addr) = interface
            .address
            .as_ref()
            .and_then(nix::sys::socket::SockaddrStorage::as_sockaddr_in)
        {
            return Ok(addr.ip());
        }
    }
    Ok(Ipv4Addr::LOCALHOST)
}
#[cfg(not(target_os = "linux"))]
// Shares a fallible signature with the Linux implementation above (whose
// interface enumeration can genuinely fail) so every call site has one
// return type instead of a `#[cfg]` dance.
#[allow(clippy::unnecessary_wraps)]
fn attach_bind_ip() -> anyhow::Result<Ipv4Addr> {
    Ok(Ipv4Addr::LOCALHOST)
}

pub(crate) struct Daemon {
    pub(crate) context: Arc<DaemonContext>,
    pub(crate) embedded: Arc<EmbeddedProviders>,
    pub(crate) state: Arc<StateStore>,
    pub(crate) inspector: Option<Arc<Inspector>>,
    pub(crate) serving: Arc<ServingCell>,
    pub(crate) resources: Arc<crate::resource_control::ResourceControl>,
    pub(crate) reconciler: OnceLock<Arc<crate::serving_reconciler::ServingReconciler>>,
    pub(crate) filesystems: OnceLock<Arc<crate::filesystem_supervisor::FilesystemSupervisor>>,
    pub(crate) vfs: Arc<omnifs_vfs::VfsServer>,
    pub(crate) bound_tcp: OnceLock<omnifs_vfs::Endpoint>,
    pub(crate) shutdown_tx: tokio::sync::watch::Sender<bool>,
    pub(crate) doctor: crate::doctor::DoctorState,
}

pub(crate) struct DaemonParts {
    pub(crate) context: Arc<DaemonContext>,
    pub(crate) embedded: Arc<EmbeddedProviders>,
    pub(crate) state: Arc<StateStore>,
    pub(crate) serving: Arc<ServingCell>,
    pub(crate) resources: Arc<crate::resource_control::ResourceControl>,
    pub(crate) inspector: Option<Arc<Inspector>>,
    pub(crate) shutdown_tx: tokio::sync::watch::Sender<bool>,
}

impl Daemon {
    pub(crate) fn new(parts: DaemonParts) -> Self {
        let DaemonParts {
            context,
            embedded,
            state,
            serving,
            resources,
            inspector,
            shutdown_tx,
        } = parts;
        let serving_dyn: Arc<dyn omnifs_vfs::ServingNamespace> = Arc::clone(&serving) as Arc<_>;
        let vfs = omnifs_vfs::VfsServer::new(serving_dyn);
        Self {
            context,
            embedded,
            state,
            inspector,
            resources,
            reconciler: OnceLock::new(),
            filesystems: OnceLock::new(),
            serving,
            vfs,
            bound_tcp: OnceLock::new(),
            shutdown_tx,
            doctor: crate::doctor::DoctorState::new(),
        }
    }

    /// Bind both namespace endpoints but keep them gated until the complete
    /// runtime is ready to enter the public control-plane state.
    pub(crate) async fn start(
        self: &Arc<Self>,
    ) -> anyhow::Result<tokio::sync::broadcast::Receiver<omnifs_vfs::ListenerEvent>> {
        let listener_events = self.vfs.listener_events();
        self.vfs.begin_startup();
        self.start_listeners().await?;
        self.vfs.mark_ready();
        anyhow::ensure!(
            self.vfs.ready(),
            "required namespace attach listener exited before readiness"
        );
        info!("namespace listeners ready");
        Ok(listener_events)
    }

    /// Run every shutdown step even after an earlier one fails, so a state
    /// store that will not close cleanly cannot mask a reconciler or drain
    /// failure (or vice versa). The first failure is the returned error;
    /// later ones are logged rather than discarded.
    ///
    /// Deliberately does not broadcast `shutdown_tx`: that is the caller's
    /// decision, not the runtime's. `build_daemon`'s startup-failure path
    /// calls this to tear down a rejected runtime while the control socket
    /// stays up for recovery; broadcasting here would tear down
    /// `ControlServer` too and the app's fresh `subscribe()` in its recovery
    /// loop would never see a signal that already fired, hanging forever on
    /// `repairs.recv()`. Every caller that wants the process to actually
    /// stop already sends the signal itself.
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.resources.shutdown();
        info!("stopping filesystem runtimes");
        let filesystem_result = match self.filesystems.get() {
            Some(supervisor) => supervisor.shutdown().await,
            None => Ok(()),
        };
        info!("filesystem runtimes stopped; stopping serving reconciler");
        let reconciler_result = match self.reconciler.get() {
            Some(reconciler) => reconciler.shutdown().await,
            None => Ok(()),
        };
        info!("serving reconciler stopped; stopping namespace listeners");
        let retired = self.serving.retire_active();
        self.vfs.shutdown().await;
        info!("namespace listeners stopped; draining active generation");
        let generation_result = match retired.drain(DRAIN_TIMEOUT).await {
            omnifs_engine::DrainOutcome::Drained => Ok(()),
            omnifs_engine::DrainOutcome::Stuck { active, .. } => Err(anyhow::anyhow!(
                "active generation retained {active} request(s) after shutdown grace"
            )),
        };
        info!("active generation drained; closing state store");
        let state_result = self.state.shutdown().await;
        info!("state store closed");

        crate::first_error([
            filesystem_result,
            reconciler_result,
            generation_result,
            state_result,
        ])
    }

    pub(crate) fn install_reconciler(
        &self,
        reconciler: Arc<crate::serving_reconciler::ServingReconciler>,
    ) -> anyhow::Result<()> {
        self.reconciler
            .set(reconciler)
            .map_err(|_| anyhow::anyhow!("serving reconciler already installed"))
    }

    pub(crate) fn install_filesystem_supervisor(
        &self,
        supervisor: Arc<crate::filesystem_supervisor::FilesystemSupervisor>,
    ) -> anyhow::Result<()> {
        self.filesystems
            .set(supervisor)
            .map_err(|_| anyhow::anyhow!("filesystem supervisor already installed"))
    }

    pub(crate) fn filesystem_supervisor(
        &self,
    ) -> anyhow::Result<&Arc<crate::filesystem_supervisor::FilesystemSupervisor>> {
        self.filesystems
            .get()
            .context("filesystem supervisor is unavailable")
    }

    pub(crate) fn provider_imported(&self, outcome: &omnifs_state::ProviderImportOutcome) {
        let repaired = outcome.disposition == omnifs_state::ProviderImportDisposition::Repaired;
        let Some(reconciler) = self.reconciler.get() else {
            tracing::warn!(
                provider = %outcome.reference.id,
                "provider preparation owner is unavailable"
            );
            return;
        };
        reconciler.provider_imported(outcome.reference.id, repaired);
    }

    async fn start_listeners(self: &Arc<Self>) -> anyhow::Result<()> {
        let vfs = &self.vfs;
        vfs.serve_unix(&self.context.attach_socket())
            .context("bind namespace Unix endpoint")?;
        let bind_ip = attach_bind_ip()?;
        let endpoint = self.bind_attach_tcp(bind_ip).await?;
        let _ = self.bound_tcp.set(endpoint);
        Ok(())
    }

    async fn bind_attach_tcp(&self, bind_ip: Ipv4Addr) -> anyhow::Result<omnifs_vfs::Endpoint> {
        if let Some(port) = self.state.attach_port().await? {
            return self.vfs.serve_tcp(bind_ip, port).with_context(|| {
                format!(
                    "bind namespace TCP endpoint on persisted port {port}; another process holds \
                     the profile's attach port"
                )
            });
        }
        for port in self.context.attach_port_candidates() {
            match self.vfs.serve_tcp(bind_ip, port) {
                Ok(endpoint) => {
                    self.state.persist_attach_port(port).await?;
                    return Ok(endpoint);
                },
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {},
                Err(error) => return Err(error).context("bind namespace TCP endpoint"),
            }
        }
        anyhow::bail!(
            "no free namespace TCP endpoint in ports {} through {}",
            ATTACH_PORT_MIN,
            ATTACH_PORT_MIN + ATTACH_PORT_COUNT - 1
        )
    }

    /// Serve until shutdown is requested. The exit of a required namespace
    /// endpoint is fatal and surfaces as an error.
    pub(crate) async fn supervise(
        &self,
        mut listener_events: tokio::sync::broadcast::Receiver<omnifs_vfs::ListenerEvent>,
    ) -> anyhow::Result<()> {
        let mut shutdown = self.shutdown_tx.subscribe();
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_ok() && *shutdown.borrow() {
                        return Ok(());
                    }
                }
                event = listener_events.recv() => match event {
                    Ok(omnifs_vfs::ListenerEvent::Exited { endpoint }) => {
                        anyhow::bail!("required namespace endpoint exited: {endpoint:?}");
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        anyhow::bail!("VFS listener supervision channel closed");
                    },
                },
            }
        }
    }

    /// One atomic view of every live mount, combining serving status with
    /// the generation provenance that dates it. Both must come from the
    /// same read of `ServingCell` or a concurrent publish can pair one
    /// generation's mounts with another generation's provenance.
    fn mount_status_views(&self) -> Vec<MountStatusView> {
        let (statuses, provenance) = self.serving.mount_statuses_with_provenance();
        statuses
            .into_iter()
            .map(|status| {
                let version = provenance
                    .mount_version(&status.name)
                    .expect("every serving mount has generation provenance");
                let (health, auth_health) =
                    mount_health_projection(status.availability, status.auth_health.as_ref());
                MountStatusView {
                    name: status.name.to_string(),
                    provider: status.provider,
                    version,
                    health,
                    auth_health,
                }
            })
            .collect()
    }

    pub(crate) fn control_status(&self) -> DaemonStatus {
        let views = self.mount_status_views();
        let mut mounts: Vec<MountInfo> = views
            .iter()
            .map(|view| MountInfo {
                mount: view.name.clone(),
                provider_name: view.provider.meta.name.to_string(),
                provider_id: view.provider.id.to_string(),
                auth_health: view.auth_health,
            })
            .collect();
        mounts.sort_by(|a, b| a.mount.cmp(&b.mount));
        let attach_serving = self.vfs.ready();
        let attach_tcp = self.attach_tcp();
        let filesystems = self.live_filesystems();
        let health = self.daemon_health(attach_serving, &filesystems, &views);
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: self.context.process_identity().pid(),
            instance_id: self.context.instance_id().to_owned(),
            executable: self.context.process_identity().executable().to_path_buf(),
            attach_tcp,
            filesystems,
            mounts,
            health: Box::new(health),
        }
    }

    fn daemon_health(
        &self,
        attach_serving: bool,
        filesystems: &[FilesystemDefinition],
        views: &[MountStatusView],
    ) -> DaemonHealth {
        DaemonHealth::new(
            HealthReport::new(
                HealthState::Healthy,
                format!(
                    "control socket serving on {}",
                    self.context.control_socket().display()
                ),
            ),
            Self::filesystem_health(attach_serving, filesystems),
            Self::mount_health_report(views),
        )
    }

    fn filesystem_health(
        attach_serving: bool,
        filesystems: &[FilesystemDefinition],
    ) -> HealthReport {
        let mut listed = vec!["attach socket local".to_string()];
        listed.extend(filesystems.iter().map(|filesystem| {
            format!(
                "attached `{}` ({}) at {} via {}",
                filesystem.name,
                filesystem.spec.protocol(),
                filesystem.spec.location().display(),
                filesystem.spec.runtime()
            )
        }));
        let listed = listed.join(", ");

        let (state, message) = if attach_serving {
            (
                HealthState::Healthy,
                format!("namespace listeners serving ({listed})"),
            )
        } else {
            (HealthState::Starting, format!("not serving ({listed})"))
        };
        HealthReport::new(state, message)
    }

    /// A mount that cannot serve degrades the rollup even when it has no
    /// credential at all: `ProviderUnavailable` must not read as healthy
    /// just because there is no auth health to report.
    fn mount_health_report(views: &[MountStatusView]) -> HealthReport {
        let degraded: Vec<&MountStatusView> = views
            .iter()
            .filter(|view| {
                !matches!(view.health, MountHealth::Active)
                    || view
                        .auth_health
                        .is_some_and(CredentialHealth::needs_attention)
            })
            .collect();
        let state = if degraded.is_empty() {
            HealthState::Healthy
        } else {
            HealthState::Degraded
        };
        let mut message = format!("{} loaded", mounts(views.len()));
        if !degraded.is_empty() {
            let detail = degraded
                .iter()
                .map(|view| {
                    format!(
                        "{}: {:?} (auth {:?})",
                        view.name, view.health, view.auth_health
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            let _ = write!(
                message,
                ", {} needing attention ({detail})",
                mounts(degraded.len())
            );
        }
        HealthReport::new(state, message)
    }

    pub(crate) fn live_mount_healths(&self) -> LiveMountHealths {
        LiveMountHealths(
            self.mount_status_views()
                .into_iter()
                .map(|view| (view.name.clone(), view))
                .collect(),
        )
    }

    pub(crate) fn attach_tcp(&self) -> Option<std::net::SocketAddr> {
        self.bound_tcp.get().and_then(|endpoint| match endpoint {
            omnifs_vfs::Endpoint::Tcp { addr } => Some(*addr),
            omnifs_vfs::Endpoint::Unix { .. } => None,
        })
    }

    pub(crate) async fn recovery(&self) -> anyhow::Result<DaemonRecovery> {
        let state = self.state.serving_state().await?;
        let durable_revision = omnifs_core::ResourceRevision::new(
            self.state.resource_snapshot().await?.revision.get(),
        );
        let serving = self.serving.provenance();
        let phase = match state.recovery {
            omnifs_state::RecoveryState::Ready => DaemonPhase::Ready,
            omnifs_state::RecoveryState::RecoveryRequired { .. } => DaemonPhase::RecoveryRequired,
        };
        Ok(DaemonRecovery {
            phase,
            durable_revision: Some(durable_revision),
            serving_revision: Some(serving.revision()),
            store_health: HealthReport::new(HealthState::Healthy, "control store available"),
            repair: None,
        })
    }

    pub(crate) async fn inventory(&self) -> anyhow::Result<DaemonInventory> {
        let recovery = self.recovery().await?;
        let status = self.control_status();
        let summaries = self.state.list_credentials().await?;
        let mut credentials = Vec::with_capacity(summaries.len());
        for summary in summaries {
            credentials.push(api_credential_status(&self.state, summary).await?);
        }
        Ok(DaemonInventory {
            info: self
                .context
                .daemon_info(Some(self.context.attach_socket()), self.attach_tcp()),
            phase: recovery.phase,
            durable_revision: recovery.durable_revision,
            serving_revision: recovery.serving_revision,
            health: *status.health,
            mounts: self.mount_records().await?,
            credentials,
            filesystems: self.live_filesystems(),
        })
    }

    pub(crate) async fn mount_records(&self) -> anyhow::Result<Vec<MountRecord>> {
        let mounts = crate::generation_builder::ResolvedDesired::load(&self.state)
            .await?
            .mounts;
        let mut health = self.live_mount_healths();
        mounts
            .into_iter()
            .map(|mount| {
                let (mount_health, auth_health) = health.take(&mount);
                api_mount_record(mount, mount_health, auth_health)
            })
            .collect()
    }

    pub(crate) async fn filesystem_status(
        &self,
        name: &omnifs_core::ResourceName,
    ) -> anyhow::Result<Option<FilesystemStatus>> {
        let desired = self
            .state
            .desired_filesystems()
            .await?
            .into_iter()
            .find(|filesystem| &filesystem.definition.name == name);
        let Some(desired) = desired else {
            return Ok(None);
        };
        let instance = self.state.filesystem_instance(name).await?;
        let phase =
            instance
                .as_ref()
                .map_or(ApiFilesystemPhase::Pending, |instance| {
                    match instance.phase {
                        omnifs_state::FilesystemPhase::Pending => ApiFilesystemPhase::Pending,
                        omnifs_state::FilesystemPhase::WaitingForNamespace => {
                            ApiFilesystemPhase::WaitingForNamespace
                        },
                        omnifs_state::FilesystemPhase::Starting => ApiFilesystemPhase::Starting,
                        omnifs_state::FilesystemPhase::Ready => ApiFilesystemPhase::Ready,
                        omnifs_state::FilesystemPhase::Stopping => ApiFilesystemPhase::Stopping,
                        omnifs_state::FilesystemPhase::Retrying => ApiFilesystemPhase::Retrying,
                        omnifs_state::FilesystemPhase::Failed => ApiFilesystemPhase::Failed,
                        omnifs_state::FilesystemPhase::Deleting => ApiFilesystemPhase::Deleting,
                    }
                });
        Ok(Some(FilesystemStatus {
            definition: desired.definition,
            desired_revision: desired.revision,
            desired_version: desired.version,
            observed_version: instance
                .as_ref()
                .and_then(|instance| instance.observed_version),
            phase,
            runtime_instance: instance
                .as_ref()
                .and_then(|instance| instance.runtime_instance.clone()),
            action_generation: instance
                .as_ref()
                .map_or(0, |instance| instance.action_generation),
            error_code: instance
                .as_ref()
                .and_then(|instance| instance.last_error_code.clone()),
            detail: instance
                .as_ref()
                .and_then(|instance| instance.last_error_detail.clone()),
            retry_at_unix_ms: instance
                .as_ref()
                .and_then(|instance| instance.retry_at)
                .and_then(|seconds| u64::try_from(seconds).ok())
                .and_then(|seconds| seconds.checked_mul(1_000)),
            deleting: instance.as_ref().is_some_and(|instance| instance.deleting),
        }))
    }

    pub(crate) async fn filesystem_access(
        &self,
        request: GetFilesystemAccessRequest,
    ) -> Result<FilesystemAccess, ControlError> {
        let status = self
            .filesystem_status(&request.filesystem)
            .await
            .map_err(|error| {
                ControlError::new(
                    ControlErrorCode::Internal,
                    format!("read filesystem status: {error:#}"),
                )
            })?
            .ok_or_else(|| {
                ControlError::new(
                    ControlErrorCode::NotFound,
                    format!("filesystem `{}` was not found", request.filesystem),
                )
            })?;
        let runtime_instance = status.runtime_instance.as_deref().ok_or_else(|| {
            ControlError::new(
                ControlErrorCode::NotReady,
                format!(
                    "filesystem `{}` has no running instance",
                    request.filesystem
                ),
            )
        })?;
        if status.phase != ApiFilesystemPhase::Ready
            || status.observed_version != Some(status.desired_version)
        {
            return Err(ControlError::new(
                ControlErrorCode::NotReady,
                format!("filesystem `{}` is not ready", request.filesystem),
            ));
        }
        let expected = omnifs_vfs::Session {
            filesystem: request.filesystem.clone(),
            spec: status.definition.spec.clone(),
            runtime_instance: runtime_instance.to_owned(),
        };
        if !self
            .vfs
            .sessions()
            .iter()
            .any(|session| session == &expected)
        {
            return Err(ControlError::new(
                ControlErrorCode::NotReady,
                format!(
                    "filesystem `{}` has no exact VFS session",
                    request.filesystem
                ),
            ));
        }
        let driver = self
            .filesystem_runtime_driver(&status.definition)
            .map_err(|error| {
                ControlError::new(
                    ControlErrorCode::Internal,
                    format!("open filesystem runtime: {error:#}"),
                )
            })?;
        let confirmed = driver.confirmed(runtime_instance).await.map_err(|error| {
            ControlError::new(
                ControlErrorCode::NotReady,
                format!("filesystem runtime identity is not ready: {error}"),
            )
        })?;
        if confirmed.is_none() {
            return Err(ControlError::new(
                ControlErrorCode::NotReady,
                format!("filesystem `{}` runtime is absent", request.filesystem),
            ));
        }
        if status.definition.spec.runtime() == omnifs_core::FilesystemRuntime::Host {
            return Ok(FilesystemAccess::HostPath(
                status.definition.spec.location().to_path_buf(),
            ));
        }
        if status.definition.spec.runtime() == omnifs_core::FilesystemRuntime::Libkrun {
            crate::fs_runtime::ensure_socat_available().map_err(|error| {
                ControlError::new(
                    ControlErrorCode::Internal,
                    format!("socat is required to open this shell: {error:#}"),
                )
            })?;
        }
        let command = driver
            .shell_command(
                request.interactive,
                request.shell.as_deref(),
                &request.command,
            )
            .expect("guest runtimes always expose a typed shell command");
        Ok(FilesystemAccess::Command(FilesystemCommand {
            program: command.get_program().to_os_string(),
            args: command.get_args().map(ToOwned::to_owned).collect(),
            current_dir: command.get_current_dir().map(ToOwned::to_owned),
        }))
    }

    fn filesystem_runtime_driver(
        &self,
        definition: &FilesystemDefinition,
    ) -> anyhow::Result<crate::fs_runtime::RuntimeDriver> {
        let runtime_paths = crate::fs_runtime::RuntimePaths::from_daemon_state(
            self.context.profile().root().to_path_buf(),
            std::env::var_os(omnifs_bootstrap::OMNIFS_HOME_ENV).is_none(),
            self.context.state_paths(),
            self.context.process_identity().executable().to_path_buf(),
        );
        crate::fs_runtime::RuntimeDriver::new(
            &runtime_paths,
            definition.name.clone(),
            definition.spec.clone(),
            crate::fs_runtime::RuntimeEventSink::discard(),
        )
    }

    pub(crate) fn live_filesystems(&self) -> Vec<FilesystemDefinition> {
        self.vfs
            .sessions()
            .into_iter()
            .map(|session| FilesystemDefinition {
                name: session.filesystem,
                spec: session.spec,
            })
            .collect()
    }
}
/// One mount's live status, derived once from an atomic
/// `ServingCell::mount_statuses_with_provenance` read. Feeds `MountInfo`
/// (control status), `MountRecord` (inventory), and the
/// top-level health rollup, so all three agree on what a mount's health is.
struct MountStatusView {
    name: String,
    provider: omnifs_core::ProviderRef,
    version: omnifs_core::MountVersion,
    health: MountHealth,
    auth_health: Option<CredentialHealth>,
}

fn mount_health_projection(
    availability: omnifs_engine::MountAvailability,
    auth_health: Option<&omnifs_auth::CredentialHealth>,
) -> (MountHealth, Option<CredentialHealth>) {
    match availability {
        omnifs_engine::MountAvailability::Active => (
            MountHealth::Active,
            auth_health.map(api_credential_health_kind),
        ),
        omnifs_engine::MountAvailability::AuthRequired => {
            (MountHealth::AuthRequired, Some(CredentialHealth::Missing))
        },
        omnifs_engine::MountAvailability::ProviderUnavailable => (
            MountHealth::ProviderUnavailable {
                reason: "provider artifact is unavailable".to_owned(),
            },
            None,
        ),
    }
}

pub(crate) struct LiveMountHealths(std::collections::HashMap<String, MountStatusView>);
impl LiveMountHealths {
    fn missing() -> (MountHealth, Option<CredentialHealth>) {
        (
            MountHealth::ProviderUnavailable {
                reason: "mount version is not in the active serving generation".to_owned(),
            },
            None,
        )
    }

    pub(crate) fn take(
        &mut self,
        mount: &crate::generation_builder::ResolvedMount,
    ) -> (MountHealth, Option<CredentialHealth>) {
        self.0
            .remove(mount.name.as_str())
            .filter(|live| live.version == mount.version)
            .map_or_else(Self::missing, |live| (live.health, live.auth_health))
    }
}

fn mounts(n: usize) -> String {
    format!("{n} mount{}", if n == 1 { "" } else { "s" })
}
