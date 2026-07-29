//! The daemon runtime: namespace endpoints, supervision, and the health
//! rollup the control plane reports.

use super::context::{ATTACH_PORT_COUNT, ATTACH_PORT_MIN, DaemonContext};
use super::provider_bundle::EmbeddedProviders;
use anyhow::Context as _;
use omnifs_api::{
    CONTROL_SHUTDOWN_DRAIN_SECS, CredentialHealth, DaemonHealth, DaemonInventory, DaemonPhase,
    DaemonRecovery, DaemonStatus, HealthReport, HealthState, MountHealth, MountInfo, MountRecord,
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
    pub(crate) manager: Arc<crate::manager::MutationManager>,
    pub(crate) vfs: Arc<omnifs_vfs::VfsServer>,
    pub(crate) bound_tcp: OnceLock<omnifs_vfs::Endpoint>,
    pub(crate) shutdown_tx: tokio::sync::watch::Sender<bool>,
}

pub(crate) struct DaemonParts {
    pub(crate) context: Arc<DaemonContext>,
    pub(crate) embedded: Arc<EmbeddedProviders>,
    pub(crate) state: Arc<StateStore>,
    pub(crate) serving: Arc<ServingCell>,
    pub(crate) manager: Arc<crate::manager::MutationManager>,
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
            manager,
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
            manager,
            serving,
            vfs,
            bound_tcp: OnceLock::new(),
            shutdown_tx,
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
    /// store that will not close cleanly cannot mask a manager or drain
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
        let manager_result = self
            .manager
            .shutdown()
            .await
            .map_err(|error| anyhow::anyhow!("mutation manager shutdown: {error}"));
        let retired = self.serving.retire_active();
        self.vfs.shutdown().await;
        let generation_result = match retired.drain(DRAIN_TIMEOUT).await {
            omnifs_engine::DrainOutcome::Drained => Ok(()),
            omnifs_engine::DrainOutcome::Stuck { active, .. } => Err(anyhow::anyhow!(
                "active generation retained {active} request(s) after shutdown grace"
            )),
        };
        let state_result = self.state.shutdown().await;

        crate::first_error([manager_result, generation_result, state_result])
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
        let filesystems = self.vfs.attachments();
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
            active_mutation: self.manager.active_mutation(),
        }
    }

    fn daemon_health(
        &self,
        attach_serving: bool,
        filesystems: &[omnifs_core::fs::Spec],
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
        filesystems: &[omnifs_core::fs::Spec],
    ) -> HealthReport {
        let mut listed = vec!["attach socket local".to_string()];
        listed.extend(filesystems.iter().map(|filesystem| {
            format!(
                "attached `{}` ({}) at {} via {}",
                filesystem.id(),
                filesystem.protocol(),
                filesystem.location().display(),
                filesystem.runtime()
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
        let durable_revision = self.state.mount_revision().await?;
        let serving = self.serving.provenance();
        let phase = match state.recovery {
            omnifs_state::RecoveryState::Ready => DaemonPhase::Ready,
            omnifs_state::RecoveryState::RecoveryRequired { .. } => DaemonPhase::RecoveryRequired,
        };
        Ok(DaemonRecovery {
            phase,
            durable_revision: Some(durable_revision),
            serving_revision: Some(serving.revision()),
            failed_mutation: state.failed_mutation,
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
            attachments: self.vfs.attachments(),
            active_mutation: status.active_mutation,
        })
    }

    pub(crate) async fn mount_records(&self) -> anyhow::Result<Vec<MountRecord>> {
        let mounts = self.state.list_mounts().await?;
        let mut health = self.live_mount_healths();
        mounts
            .into_iter()
            .map(|mount| {
                let (mount_health, auth_health) = health.take(&mount);
                api_mount_record(mount, mount_health, auth_health)
            })
            .collect()
    }
}
/// One mount's live status, derived once from an atomic
/// `ServingCell::mount_statuses_with_provenance` read. Feeds `MountInfo`
/// (control status), `MountRecord` (inventory and `GetMount`), and the
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
        mount: &omnifs_state::StoredMount,
    ) -> (MountHealth, Option<CredentialHealth>) {
        self.0
            .remove(mount.document.name.as_str())
            .filter(|live| live.version == mount.version)
            .map_or_else(Self::missing, |live| (live.health, live.auth_health))
    }
}

fn mounts(n: usize) -> String {
    format!("{n} mount{}", if n == 1 { "" } else { "s" })
}
