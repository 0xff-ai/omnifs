//! Durable frontend desired-state controller.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use clap::{Args, ValueEnum};
use omnifs_mtab::{MountKind, MountState};
use omnifs_workspace::config::{
    ConfigDocument, EffectiveFrontend, Environment, Filesystem, FrontendId, FrontendPlan,
    FrontendSpec, HostOs,
};
use omnifs_workspace::layout::{WorkspaceLayout, resolve_mount_point};
use omnifs_workspace::runtime_record::{FrontendKind, RuntimeRecord, Via};

use crate::commands::receipt::FrontendReceipt;
use crate::frontend_backend::{DockerBackend, FrontendBackend};
use crate::frontend_container::{frontend_container_name, resolve_frontend_image};
use crate::inventory::Inventory;
use crate::krunkit_backend::{GuestImageSource, KrunkitBackend};
use crate::launch_backend::{DockerTarget, GUEST_MOUNT};
use crate::local_backend::LocalBackend;
use crate::runtime::Runtime;
use crate::ui::output::{Output, ResultVerdict};
use crate::workspace::Workspace;
use serde::Serialize;

const DOCKER_TIMEOUT: Duration = Duration::from_secs(5);
const KRUNKIT_TIMEOUT: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(200);
// The wire client's reconnect backoff tops out at two seconds. Allow more than
// one ceiling interval before deciding a desired local runner is absent.
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FrontendFilesystem {
    Fuse,
    Nfs,
}

impl From<FrontendFilesystem> for Filesystem {
    fn from(value: FrontendFilesystem) -> Self {
        match value {
            FrontendFilesystem::Fuse => Self::Fuse,
            FrontendFilesystem::Nfs => Self::Nfs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FrontendEnvironment {
    Host,
    Docker,
    Krunkit,
}

impl From<FrontendEnvironment> for Environment {
    fn from(value: FrontendEnvironment) -> Self {
        match value {
            FrontendEnvironment::Host => Self::Host,
            FrontendEnvironment::Docker => Self::Docker,
            FrontendEnvironment::Krunkit => Self::Krunkit,
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct FrontendEnableArgs {
    #[arg(value_enum)]
    pub filesystem: FrontendFilesystem,
    #[arg(long, value_enum)]
    pub environment: FrontendEnvironment,
    #[arg(long)]
    pub location: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct FrontendDisableArgs {
    #[arg(value_enum)]
    pub filesystem: FrontendFilesystem,
    #[arg(long, value_enum)]
    pub environment: FrontendEnvironment,
    #[arg(long)]
    pub location: Option<PathBuf>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendRestartArgs {
    #[arg(value_enum)]
    pub filesystem: Option<FrontendFilesystem>,
    #[arg(long, value_enum)]
    pub environment: Option<FrontendEnvironment>,
    #[arg(long)]
    pub location: Option<PathBuf>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendLsArgs {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    Stopped,
    Attached,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnableAction {
    Stopped,
    Attached,
    Launch,
}

fn enable_action(daemon_running: bool, attached: bool) -> EnableAction {
    if !daemon_running {
        EnableAction::Stopped
    } else if attached {
        EnableAction::Attached
    } else {
        EnableAction::Launch
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrontendResult {
    pub id: FrontendId,
    pub state: RuntimeState,
    pub changed: bool,
    pub fix: Option<String>,
    pub detail: Option<String>,
}

pub struct FrontendController<'a> {
    workspace: &'a Workspace,
    document: ConfigDocument,
    plan: FrontendPlan,
    host_os: HostOs,
    default_location: PathBuf,
    output: Output,
}

impl<'a> FrontendController<'a> {
    pub fn new(workspace: &'a Workspace, output: Output) -> Result<Self> {
        let default_location =
            resolve_mount_point().unwrap_or_else(|| workspace.layout().config_dir.join("omnifs"));
        let document = ConfigDocument::load(workspace.layout().config_file.clone())?;
        let host_os = current_host_os();
        let plan = document.config().frontends.clone();
        Ok(Self {
            workspace,
            document,
            plan,
            host_os,
            default_location,
            output,
        })
    }

    pub fn desired(&self) -> Result<Vec<EffectiveFrontend>, omnifs_workspace::config::ConfigError> {
        self.plan.effective(self.host_os, &self.default_location)
    }

    pub async fn enable(&mut self, args: FrontendEnableArgs) -> Result<FrontendResult> {
        let spec = FrontendSpec {
            filesystem: args.filesystem.into(),
            environment: args.environment.into(),
            location: args.location,
        };
        let mut candidate = self.plan.clone();
        let (changed, id) = candidate.enable(spec, self.host_os, &self.default_location)?;
        if changed {
            self.plan = candidate;
            self.document.replace_frontends(&self.plan)?;
            self.document.save()?;
        }
        if !changed {
            let inventory = self.observed_inventory().await?;
            let status = inventory.daemon.status.clone();
            let attached = observed_entries(&inventory)
                .iter()
                .any(|frontend| frontend.id() == id);
            match enable_action(status.is_some(), attached) {
                EnableAction::Stopped => {
                    return Ok(FrontendResult {
                        id,
                        state: RuntimeState::Stopped,
                        changed: false,
                        fix: Some("omnifs up".to_owned()),
                        detail: None,
                    });
                },
                EnableAction::Attached => {
                    return Ok(FrontendResult {
                        id,
                        state: RuntimeState::Attached,
                        changed: false,
                        fix: None,
                        detail: None,
                    });
                },
                EnableAction::Launch => {
                    let target = self
                        .desired()?
                        .into_iter()
                        .find(|entry| entry.id() == id)
                        .context("enabled frontend disappeared from plan")?;
                    let mut result = self.launch_result(target).await;
                    result.changed = false;
                    return Ok(result);
                },
            }
        }
        let inventory = self.observed_inventory().await?;
        let Some(_status) = inventory.daemon.status.as_ref() else {
            return Ok(FrontendResult {
                id,
                state: RuntimeState::Stopped,
                changed: true,
                fix: Some("omnifs up".into()),
                detail: None,
            });
        };
        let target = self
            .desired()?
            .into_iter()
            .find(|entry| entry.id() == id)
            .context("enabled frontend disappeared from plan")?;
        Ok(self.launch_result(target).await)
    }

    pub async fn disable(&mut self, args: FrontendDisableArgs) -> Result<FrontendResult> {
        let filesystem = args.filesystem.into();
        let environment = args.environment.into();
        let location = args.location;
        let inventory = self.observed_inventory().await?;
        let status = inventory.daemon.status.clone();
        let desired = self.desired()?;
        let observed = observed_entries(&inventory);
        let fully_specified = location.is_some() || environment != Environment::Host;
        let id = match resolve_selector(
            desired.clone(),
            observed.clone(),
            Some(filesystem),
            Some(environment),
            location.as_deref(),
        )? {
            Some(entry) => entry.id(),
            None if fully_specified => {
                let spec = FrontendSpec {
                    filesystem,
                    environment,
                    location,
                };
                spec.resolve(self.host_os, &self.default_location)?.id()
            },
            None => bail!("no frontend matches the selector"),
        };
        let configured = desired.iter().any(|entry| entry.id() == id);
        if configured {
            self.plan
                .disable(&id, self.host_os, &self.default_location)?;
            self.document.replace_frontends(&self.plan)?;
            self.document.save()?;
        }
        let Some(_) = status else {
            return Ok(FrontendResult {
                id,
                state: RuntimeState::Stopped,
                changed: configured,
                fix: None,
                detail: None,
            });
        };
        let attached = observed.iter().any(|entry| entry.id() == id);
        if !attached {
            return Ok(FrontendResult {
                id,
                state: RuntimeState::Stopped,
                changed: configured,
                fix: None,
                detail: None,
            });
        }
        match self.stop(&id).await {
            Ok(()) => Ok(FrontendResult {
                id: id.clone(),
                state: RuntimeState::Stopped,
                changed: true,
                fix: None,
                detail: None,
            }),
            Err(error) => Ok(FrontendResult {
                id: id.clone(),
                state: RuntimeState::Failed,
                changed: true,
                fix: Some(disable_fix(&id)),
                detail: Some(error.to_string()),
            }),
        }
    }

    pub async fn restart(&mut self, args: FrontendRestartArgs) -> Result<Vec<FrontendResult>> {
        let inventory = self.observed_inventory().await?;
        let status = inventory.daemon.status.clone();
        let desired = self.desired()?;
        let observed = observed_entries(&inventory);
        let no_selector =
            args.filesystem.is_none() && args.environment.is_none() && args.location.is_none();
        let targets = if no_selector {
            if desired.is_empty() {
                bail!("no desired frontend matches the selector");
            }
            desired
        } else {
            vec![
                resolve_selector(
                    desired,
                    observed.clone(),
                    args.filesystem.map(Into::into),
                    args.environment.map(Into::into),
                    args.location.as_deref(),
                )?
                .context("no frontend matches the selector")?,
            ]
        };
        if status.is_none() {
            return Ok(targets
                .into_iter()
                .map(|target| stopped_restart_result(target.id()))
                .collect());
        }
        let mut results = Vec::with_capacity(targets.len());
        for target in targets {
            let id = target.id();
            let attached = observed.iter().any(|entry| entry.id() == id);
            let result = if attached {
                match self.stop(&id).await {
                    Ok(()) => self.launch_result(target).await,
                    Err(error) => FrontendResult {
                        id,
                        state: RuntimeState::Failed,
                        changed: true,
                        fix: Some(restart_fix(&target)),
                        detail: Some(error.to_string()),
                    },
                }
            } else {
                self.launch_result(target).await
            };
            results.push(result);
        }
        Ok(results)
    }

    /// Converge the durable frontend plan after `up` applies desired mount
    /// state. Existing frontend processes are never replaced as a consequence
    /// of daemon startup: they get a bounded reconnect window, and only a
    /// runner proven absent is launched.
    pub async fn converge(&self, daemon_restarted: bool) -> Result<Vec<FrontendResult>> {
        let targets = self.desired()?;
        let mut attached = self.attached_ids().await?;
        if daemon_restarted {
            let deadline = tokio::time::Instant::now() + RECONNECT_TIMEOUT;
            while targets
                .iter()
                .any(|target| !attached.contains(&target.id()))
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(POLL).await;
                attached = self.attached_ids().await?;
            }
        }

        let mut failures = Vec::new();
        for target in &targets {
            if attached.contains(&target.id()) {
                continue;
            }
            if daemon_restarted {
                match self.runner_is_running(target).await {
                    Ok(true) => {
                        failures.push(FrontendResult {
                            id: target.id(),
                            state: RuntimeState::Failed,
                            changed: false,
                            fix: Some(restart_fix(target)),
                            detail: Some(
                                "frontend process is still running but did not reconnect to the new daemon"
                                    .to_owned(),
                            ),
                        });
                        continue;
                    },
                    Ok(false) => {},
                    Err(error) => {
                        failures.push(FrontendResult {
                            id: target.id(),
                            state: RuntimeState::Failed,
                            changed: false,
                            fix: Some(restart_fix(target)),
                            detail: Some(format!("inspect existing frontend process: {error:#}")),
                        });
                        continue;
                    },
                }
            }
            if let Err(error) = self.launch(target).await {
                failures.push(FrontendResult {
                    id: target.id(),
                    state: RuntimeState::Failed,
                    changed: false,
                    fix: Some(restart_fix(target)),
                    detail: Some(format!("{error:#}")),
                });
            }
        }
        Ok(failures)
    }

    async fn observed_inventory(&self) -> Result<Inventory> {
        Inventory::collect(self.workspace).await
    }

    async fn runner_is_running(&self, target: &EffectiveFrontend) -> Result<bool> {
        match target.environment {
            Environment::Host => {
                let location = target
                    .location
                    .as_deref()
                    .context("host frontend has no location")?;
                let state_dir = self.workspace.layout().frontend_state_dir(
                    match target.filesystem {
                        Filesystem::Fuse => FrontendKind::Fuse,
                        Filesystem::Nfs => FrontendKind::Nfs,
                    },
                    location,
                );
                if !state_dir.try_exists()? {
                    return Ok(false);
                }
                let state = MountState::read_unique(&state_dir)?;
                let same_kind = matches!(
                    (target.filesystem, &state.kind),
                    (Filesystem::Fuse, MountKind::Fuse) | (Filesystem::Nfs, MountKind::Nfs { .. })
                );
                Ok(state.mount_point == location
                    && same_kind
                    && crate::host_teardown::local_mount_is_owned(&state))
            },
            Environment::Docker => {
                let name = frontend_container_name(self.workspace.layout())?;
                let image = resolve_frontend_image(None, self.document.config())?;
                let target =
                    DockerTarget::new(name.as_str().to_owned(), image.as_str().to_owned())?;
                let runtime = Runtime::connect_for(&target, self.output)?;
                Ok(DockerBackend::new(runtime)
                    .is_running()
                    .await?
                    .unwrap_or(false))
            },
            Environment::Krunkit => Ok(KrunkitBackend::new(
                self.workspace.layout().config_dir.clone(),
            )
            .is_running()
            .await?
            .unwrap_or(false)),
        }
    }

    async fn attached_ids(&self) -> Result<Vec<FrontendId>> {
        let inventory = self.observed_inventory().await?;
        Ok(observed_entries(&inventory)
            .into_iter()
            .map(|entry| entry.id())
            .collect())
    }

    async fn launch_result(&self, target: EffectiveFrontend) -> FrontendResult {
        let id = target.id();
        match self.launch(&target).await {
            Ok(()) => FrontendResult {
                id,
                state: RuntimeState::Attached,
                changed: true,
                fix: None,
                detail: None,
            },
            Err(error) => FrontendResult {
                id,
                state: RuntimeState::Failed,
                changed: true,
                fix: Some(restart_fix(&target)),
                detail: Some(error.to_string()),
            },
        }
    }

    async fn launch(&self, entry: &EffectiveFrontend) -> Result<()> {
        // A frontend serves the shared namespace, including an empty one. The
        // first mount is only a readiness probe target; it is not frontend
        // identity or ownership, and absence must not block launch.
        let mount = self
            .workspace
            .mounts()?
            .into_iter()
            .map(|m| m.name.to_string())
            .next();
        let paths = self.workspace.layout().clone();
        match entry.environment {
            Environment::Host => {
                let point = entry
                    .location
                    .clone()
                    .context("host frontend has no location")?;
                let backend = LocalBackend::new(
                    paths,
                    point,
                    match entry.filesystem {
                        Filesystem::Fuse => FrontendKind::Fuse,
                        Filesystem::Nfs => FrontendKind::Nfs,
                    }
                    .into(),
                )?;
                backend.launch(mount.as_deref()).await
            },
            Environment::Docker => self.launch_docker(&paths, mount.as_deref()).await,
            Environment::Krunkit => self.launch_krunkit(&paths, mount.as_deref()).await,
        }
    }

    async fn launch_docker(&self, paths: &WorkspaceLayout, mount: Option<&str>) -> Result<()> {
        let config = self.document.config();
        let image = resolve_frontend_image(None, config)?;
        let name = frontend_container_name(paths)?;
        let target = DockerTarget::new(name.as_str().to_owned(), image.as_str().to_owned())?;
        let runtime =
            Runtime::connect_ready(&target, "omnifs frontend enable", self.output).await?;
        #[cfg(target_os = "linux")]
        let (bind_ip, expected) = {
            let ip = runtime.frontend_attach_bind_ip().await?;
            (Some(ip), ip)
        };
        #[cfg(not(target_os = "linux"))]
        let (bind_ip, expected) = (None, std::net::Ipv4Addr::LOCALHOST);
        let attach = self
            .workspace
            .daemon()
            .frontend_attach_target(bind_ip)
            .await?;
        let addr: SocketAddr = attach
            .addr
            .parse()
            .context("invalid daemon attach address")?;
        ensure!(
            addr.ip() == IpAddr::V4(expected),
            "daemon attach listener is bound to {}; restart daemon",
            addr.ip()
        );
        let backend = DockerBackend::new(runtime);
        backend
            .launch(&paths.config_dir, addr.port(), &attach.token)
            .await?;
        wait_for_mount(&backend, mount, DOCKER_TIMEOUT).await
    }

    async fn launch_krunkit(&self, paths: &WorkspaceLayout, mount: Option<&str>) -> Result<()> {
        let image = GuestImageSource::resolve(None, self.document.config())?
            .into_local_path(&paths.cache_dir, self.output)
            .await?;
        let attach = self
            .workspace
            .daemon()
            .frontend_attach_target_vsock()
            .await?;
        let backend = KrunkitBackend::new(paths.config_dir.clone());
        backend
            .launch(Path::new(&attach.socket_path), &attach.token, image)
            .await?;
        wait_for_mount(&backend, mount, KRUNKIT_TIMEOUT).await
    }

    async fn stop(&self, id: &FrontendId) -> Result<()> {
        match id.environment() {
            Environment::Host => {
                let Some(location) = id.location() else {
                    bail!("host frontend requires a location")
                };
                crate::host_teardown::teardown_local_frontend(
                    &self.workspace.layout().frontend_state_root(),
                    location,
                    id.filesystem() == Filesystem::Nfs,
                )
            },
            Environment::Docker => {
                let name = frontend_container_name(self.workspace.layout())?;
                let image = resolve_frontend_image(None, self.document.config())?;
                let target =
                    DockerTarget::new(name.as_str().to_owned(), image.as_str().to_owned())?;
                let runtime = Runtime::connect_for(&target, self.output)?;
                DockerBackend::new(runtime).tear_down().await
            },
            Environment::Krunkit => {
                KrunkitBackend::new(self.workspace.layout().config_dir.clone())
                    .tear_down()
                    .await
            },
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct FrontendTeardownReport {
    pub found: bool,
    pub failures: Vec<String>,
}

impl FrontendTeardownReport {
    pub fn error(&self) -> Option<String> {
        (!self.failures.is_empty()).then(|| self.failures.join("; "))
    }
}

pub(crate) async fn teardown_all(
    paths: &WorkspaceLayout,
    force: bool,
    output: Output,
) -> FrontendTeardownReport {
    let mut report = FrontendTeardownReport::default();
    // Guest delivery is owned by the daemon runtime record. When no record
    // exists, there is no workspace-owned guest frontend to inspect, so an
    // idempotent `down` must not turn an unavailable Docker/krunkit runtime
    // into a false failure. A malformed record stays fail-closed and probes the
    // backends so its corruption remains visible to the caller.
    let guest_expected = match RuntimeRecord::read(&paths.runtime_record_file()) {
        Ok(Some(record)) => record
            .frontends
            .iter()
            .any(|frontend| matches!(frontend.via, Via::Docker | Via::Krunkit)),
        Ok(None) => false,
        Err(_) => true,
    };
    if guest_expected {
        let krunkit = KrunkitBackend::new(paths.config_dir.clone());
        match krunkit.is_running().await {
            Ok(Some(_)) => {
                report.found = true;
                if let Err(error) = krunkit.tear_down().await {
                    report
                        .failures
                        .push(format!("remove krunkit frontend: {error:#}"));
                }
            },
            Ok(None) => {},
            Err(error) => report
                .failures
                .push(format!("inspect krunkit frontend: {error:#}")),
        }
        match frontend_container_name(paths).and_then(|name| {
            DockerTarget::new(
                name.as_str().to_owned(),
                crate::frontend_container::FRONTEND_DEV_IMAGE.to_owned(),
            )
        }) {
            Ok(target) => match Runtime::connect_for(&target, output) {
                Ok(runtime) => match runtime.frontend_container_for_home(&paths.config_dir).await {
                    Ok(Some(name)) => {
                        report.found = true;
                        match DockerTarget::new(
                            name.as_str().to_owned(),
                            crate::frontend_container::FRONTEND_DEV_IMAGE.to_owned(),
                        )
                        .and_then(|target| Runtime::connect_for(&target, output))
                        {
                            Ok(runtime) => {
                                if let Err(error) = DockerBackend::new(runtime).tear_down().await {
                                    report
                                        .failures
                                        .push(format!("remove Docker frontend: {error:#}"));
                                }
                            },
                            Err(error) => report
                                .failures
                                .push(format!("connect Docker frontend: {error:#}")),
                        }
                    },
                    Ok(None) => {},
                    Err(error) => report
                        .failures
                        .push(format!("inspect Docker frontend: {error:#}")),
                },
                Err(error) => report
                    .failures
                    .push(format!("Docker not reachable: {error:#}")),
            },
            Err(error) => report
                .failures
                .push(format!("resolve Docker frontend: {error:#}")),
        }
    }
    #[cfg(feature = "daemon")]
    match crate::host_teardown::teardown_local_frontends(&paths.frontend_state_root(), force) {
        Ok(summary) => {
            report.found |= summary.unmounted > 0 || summary.swept_orphans > 0;
            report
                .failures
                .extend(summary.failed.into_iter().map(|failure| {
                    format!(
                        "could not unmount {}: {}",
                        failure.mount_point.display(),
                        failure.reason
                    )
                }));
            report.failures.extend(summary.errors);
        },
        Err(error) => report
            .failures
            .push(format!("inspect local frontend state: {error:#}")),
    }
    report
}

async fn wait_for_mount(
    backend: &impl FrontendBackend,
    mount: Option<&str>,
    timeout: Duration,
) -> Result<()> {
    let path = mount_probe_path(mount);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match backend.mount_ready(&path).await {
            Ok(true) => return Ok(()),
            Ok(false) => {},
            Err(error) => {
                let cleanup = backend.tear_down().await.err();
                return Err(match cleanup {
                    Some(cleanup) => {
                        error.context(format!("frontend cleanup also failed: {cleanup:#}"))
                    },
                    None => error,
                });
            },
        }
        if tokio::time::Instant::now() >= deadline {
            let message = format!(
                "{path} did not appear inside the frontend within {}s",
                timeout.as_secs()
            );
            match backend.tear_down().await {
                Ok(()) => bail!(message),
                Err(cleanup) => bail!("{message}; frontend cleanup also failed: {cleanup:#}"),
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

impl FrontendEnableArgs {
    pub async fn run(self, output: Output) -> Result<crate::error::ExitCode> {
        let workspace = Workspace::resolve()?;
        let mut controller = FrontendController::new(&workspace, output)?;
        let result = controller.enable(self).await?;
        let inventory = crate::inventory::Inventory::collect(&workspace).await?;
        let receipt = FrontendReceipt::from_inventory(&inventory, vec![result]);
        finish_receipt(output, &receipt)
    }
}
impl FrontendDisableArgs {
    pub async fn run(self, output: Output) -> Result<crate::error::ExitCode> {
        let workspace = Workspace::resolve()?;
        let mut controller = FrontendController::new(&workspace, output)?;
        let result = controller.disable(self).await?;
        let inventory = crate::inventory::Inventory::collect(&workspace).await?;
        let receipt = FrontendReceipt::from_inventory(&inventory, vec![result]);
        finish_receipt(output, &receipt)
    }
}
impl FrontendRestartArgs {
    pub async fn run(self, output: Output) -> Result<crate::error::ExitCode> {
        let workspace = Workspace::resolve()?;
        let mut controller = FrontendController::new(&workspace, output)?;
        let results = controller.restart(self).await?;
        let inventory = crate::inventory::Inventory::collect(&workspace).await?;
        let receipt = FrontendReceipt::from_inventory(&inventory, results);
        finish_receipt(output, &receipt)
    }
}
impl FrontendLsArgs {
    pub async fn run(self, output: Output) -> Result<crate::error::ExitCode> {
        let ws = Workspace::resolve()?;
        let inventory = crate::inventory::Inventory::collect(&ws).await?;
        let exit = if inventory.verdict() == crate::inventory::Verdict::Degraded {
            crate::error::ExitCode::Degraded
        } else {
            crate::error::ExitCode::Success
        };
        if output.is_structured() {
            #[derive(serde::Serialize)]
            struct FrontendList {
                frontends: Vec<crate::inventory::FrontendStatus>,
                verdict: crate::inventory::Verdict,
            }
            output.emit_result(
                ResultVerdict::from(inventory.verdict()),
                FrontendList {
                    frontends: inventory.frontends.clone(),
                    verdict: inventory.verdict(),
                },
            )?;
        } else {
            let mut report = crate::ui::table::Report::new();
            report.push(crate::ui::table::Block::Resources(
                crate::status::frontend_table(&inventory.frontends),
            ));
            report.print();
        }
        Ok(exit)
    }
}

fn finish_receipt(output: Output, receipt: &FrontendReceipt) -> Result<crate::error::ExitCode> {
    if output.is_structured() {
        output.emit_result(receipt.output_verdict(), receipt)?;
    } else {
        render_frontend_receipt(receipt);
        for result in receipt
            .rows
            .iter()
            .filter(|result| result.state == RuntimeState::Failed)
        {
            output.narrate(format_failure(result));
        }
    }
    Ok(receipt.exit_code())
}

fn render_frontend_receipt(receipt: &FrontendReceipt) {
    let changed = receipt.rows.iter().filter(|row| row.changed).count();
    let mut frontends = crate::status::frontend_table(&receipt.frontends);
    frontends.count = crate::ui::table::CountLabel::named(changed, "changed");

    let mut report = crate::ui::table::Report::new();
    report.push(crate::ui::table::Block::Resources(frontends));
    if !receipt.access_paths.is_empty() {
        report.push(crate::ui::table::Block::Resources(frontend_access_table(
            &receipt.access_paths,
        )));
    }
    report.print();
}

fn frontend_access_table(
    paths: &[crate::inventory::AccessPath],
) -> crate::ui::table::ResourceTable {
    let mut table = crate::ui::table::ResourceTable::new(
        "Access paths",
        paths.len(),
        vec![
            crate::ui::table::Column::new(
                "Filesystem",
                crate::ui::table::Priority::Identity,
                crate::ui::table::WidthPolicy::Auto,
            ),
            crate::ui::table::Column::new(
                "Environment",
                crate::ui::table::Priority::Identity,
                crate::ui::table::WidthPolicy::Auto,
            ),
            crate::ui::table::Column::new(
                "Path",
                crate::ui::table::Priority::Essential,
                crate::ui::table::WidthPolicy::Path,
            ),
            crate::ui::table::Column::new(
                "State",
                crate::ui::table::Priority::Essential,
                crate::ui::table::WidthPolicy::Auto,
            ),
        ],
    );
    for path in paths {
        let state = match path.state {
            crate::inventory::AccessState::Available => {
                crate::ui::table::StateToken::positive(path.state.label())
            },
            crate::inventory::AccessState::Offline => {
                crate::ui::table::StateToken::neutral(path.state.label())
            },
            crate::inventory::AccessState::Failed => {
                crate::ui::table::StateToken::failure(path.state.label())
            },
        };
        table.push(crate::ui::table::ResourceRow::new(
            [
                crate::ui::table::Cell::new(path.filesystem.label()),
                crate::ui::table::Cell::new(path.environment.label()),
                crate::ui::table::Cell::new(path.path.display().to_string()),
                crate::ui::table::Cell::state(state.clone()),
            ],
            state,
        ));
    }
    table
}

fn format_failure(result: &FrontendResult) -> String {
    format!(
        "Frontend {} failed: {}\nFix  {}",
        result.id,
        result.detail.as_deref().unwrap_or("unknown error"),
        result.fix.as_deref().unwrap_or("omnifs frontend restart")
    )
}

fn restart_fix(target: &EffectiveFrontend) -> String {
    let mut fix = format!(
        "omnifs frontend restart {} --environment {}",
        target.filesystem, target.environment
    );
    if let Some(location) = &target.location {
        fix.push_str(" --location ");
        fix.push_str(&location.to_string_lossy());
    }
    fix
}

fn disable_fix(id: &FrontendId) -> String {
    let mut fix = format!(
        "omnifs frontend disable {} --environment {}",
        id.filesystem(),
        id.environment()
    );
    if let Some(location) = id.location() {
        fix.push_str(" --location ");
        fix.push_str(&location.to_string_lossy());
    }
    fix
}

fn stopped_restart_result(id: FrontendId) -> FrontendResult {
    FrontendResult {
        id,
        state: RuntimeState::Stopped,
        changed: false,
        fix: Some("omnifs up".to_owned()),
        detail: None,
    }
}

fn observed_entries(inventory: &Inventory) -> Vec<EffectiveFrontend> {
    inventory
        .frontends
        .iter()
        .map(|frontend| EffectiveFrontend {
            filesystem: frontend.filesystem,
            environment: frontend.environment,
            location: frontend.location.clone(),
            source: omnifs_workspace::config::PlanSource::Configured,
        })
        .collect()
}

fn resolve_selector(
    desired: Vec<EffectiveFrontend>,
    observed: Vec<EffectiveFrontend>,
    filesystem: Option<Filesystem>,
    environment: Option<Environment>,
    location: Option<&Path>,
) -> Result<Option<EffectiveFrontend>> {
    let mut entries = desired;
    for entry in observed {
        if !entries.iter().any(|existing| existing.id() == entry.id()) {
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| entry.id().to_string());
    let matches = entries
        .into_iter()
        .filter(|entry| {
            filesystem.is_none_or(|value| entry.filesystem == value)
                && environment.is_none_or(|value| entry.environment == value)
                && location.is_none_or(|value| entry.location.as_deref() == Some(value))
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [entry] => Ok(Some(entry.clone())),
        _ => bail!(
            "selector matches multiple frontends: {}",
            matches
                .iter()
                .map(|entry| entry.id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn mount_probe_path(mount: Option<&str>) -> String {
    mount.map_or_else(
        || GUEST_MOUNT.to_owned(),
        |name| format!("{GUEST_MOUNT}/{name}"),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn host(filesystem: Filesystem, location: &str) -> EffectiveFrontend {
        EffectiveFrontend {
            filesystem,
            environment: Environment::Host,
            location: Some(PathBuf::from(location)),
            source: omnifs_workspace::config::PlanSource::Configured,
        }
    }

    #[test]
    fn selector_uses_exact_host_location() {
        let selected = resolve_selector(
            vec![host(Filesystem::Nfs, "/a"), host(Filesystem::Nfs, "/b")],
            Vec::new(),
            Some(Filesystem::Nfs),
            Some(Environment::Host),
            Some(Path::new("/b")),
        )
        .unwrap()
        .expect("one match");
        assert_eq!(selected.location.as_deref(), Some(Path::new("/b")));
    }

    #[test]
    fn selector_unions_and_deduplicates_desired_and_observed() {
        let desired = host(Filesystem::Nfs, "/a");
        let selected = resolve_selector(
            vec![desired.clone()],
            vec![desired],
            Some(Filesystem::Nfs),
            Some(Environment::Host),
            None,
        )
        .unwrap()
        .expect("deduplicated identity");
        assert_eq!(selected.location.as_deref(), Some(Path::new("/a")));
    }

    #[test]
    fn partial_selector_rejects_ambiguity_and_missing_identity() {
        let entries = vec![host(Filesystem::Nfs, "/a"), host(Filesystem::Nfs, "/b")];
        let ambiguous = resolve_selector(
            entries.clone(),
            Vec::new(),
            Some(Filesystem::Nfs),
            Some(Environment::Host),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(ambiguous.contains("multiple frontends"));

        assert!(
            resolve_selector(
                entries,
                Vec::new(),
                Some(Filesystem::Fuse),
                Some(Environment::Host),
                None,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn stopped_restart_is_noop() {
        let id = FrontendPlan::default()
            .effective(HostOs::Linux, "/a")
            .expect("default identity")
            .into_iter()
            .next()
            .expect("default frontend")
            .id();
        let stopped = stopped_restart_result(id);
        assert_eq!(stopped.fix.as_deref(), Some("omnifs up"));
        assert!(stopped.detail.is_none());
    }

    #[test]
    fn unchanged_enable_decides_runtime_action() {
        assert_eq!(enable_action(false, false), EnableAction::Stopped);
        assert_eq!(enable_action(true, true), EnableAction::Attached);
        assert_eq!(enable_action(true, false), EnableAction::Launch);
    }

    #[test]
    fn empty_namespace_uses_frontend_root_as_readiness_probe() {
        assert_eq!(mount_probe_path(None), GUEST_MOUNT);
        assert_eq!(mount_probe_path(Some("notes")), "/omnifs/notes");
    }

    struct ProbeBackend {
        torn_down: Arc<AtomicUsize>,
        cleanup_fails: bool,
    }

    impl FrontendBackend for ProbeBackend {
        async fn mount_ready(&self, _path: &str) -> Result<bool> {
            anyhow::bail!("probe failed")
        }

        async fn is_running(&self) -> Result<Option<bool>> {
            Ok(Some(true))
        }

        async fn tear_down(&self) -> Result<()> {
            self.torn_down.fetch_add(1, Ordering::SeqCst);
            if self.cleanup_fails {
                anyhow::bail!("cleanup failed")
            }
            Ok(())
        }

        fn shell_command(&self, _shell_override: Option<&str>, _trailing: &[String]) -> Command {
            Command::new("true")
        }
    }

    #[tokio::test]
    async fn probe_error_tears_down_and_keeps_cleanup_context() {
        let backend = ProbeBackend {
            torn_down: Arc::new(AtomicUsize::new(0)),
            cleanup_fails: true,
        };
        let error = wait_for_mount(&backend, None, Duration::from_secs(1))
            .await
            .expect_err("probe should fail");
        assert_eq!(backend.torn_down.load(Ordering::SeqCst), 1);
        assert!(error.to_string().contains("cleanup also failed"));
    }
}
