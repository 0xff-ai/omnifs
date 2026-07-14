//! Imperative lifecycle for independent frontend runners.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use clap::{Args, ValueEnum};
use omnifs_mtab::{MountKind, MountState};
use omnifs_workspace::layout::{WorkspaceLayout, resolve_mount_point};
use omnifs_workspace::runtime_record::FrontendKind;
use serde::{Deserialize, Serialize};

use crate::commands::receipt::FrontendReceipt;
use crate::frontend_backend::{DockerBackend, FrontendBackend};
use crate::frontend_container::{frontend_container_name, resolve_frontend_image};
use crate::inventory::{FrontendState, FrontendStatus, Inventory};
use crate::krunkit_backend::{GuestImageSource, KrunkitBackend};
use crate::launch_backend::{DockerTarget, GUEST_MOUNT};
use crate::local_backend::LocalBackend;
use crate::runtime::Runtime;
use crate::ui::output::{Output, ResultVerdict};
use crate::workspace::Workspace;

const DOCKER_TIMEOUT: Duration = Duration::from_secs(5);
const KRUNKIT_TIMEOUT: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(200);
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, ValueEnum, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum FrontendFilesystem {
    Fuse,
    Nfs,
}

impl FrontendFilesystem {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fuse => "fuse",
            Self::Nfs => "nfs",
        }
    }
}

impl std::fmt::Display for FrontendFilesystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, ValueEnum, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum FrontendEnvironment {
    Host,
    Docker,
    Krunkit,
}

impl FrontendEnvironment {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::Krunkit => "krunkit",
        }
    }
}

impl std::fmt::Display for FrontendEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct FrontendId {
    filesystem: FrontendFilesystem,
    environment: FrontendEnvironment,
    location: Option<PathBuf>,
}

impl FrontendId {
    pub fn new(
        filesystem: FrontendFilesystem,
        environment: FrontendEnvironment,
        location: Option<PathBuf>,
    ) -> Self {
        Self {
            filesystem,
            environment,
            location,
        }
    }
    pub const fn filesystem(&self) -> FrontendFilesystem {
        self.filesystem
    }
    pub const fn environment(&self) -> FrontendEnvironment {
        self.environment
    }
    pub fn location(&self) -> Option<&Path> {
        self.location.as_deref()
    }
}

impl std::fmt::Display for FrontendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.filesystem, self.environment)?;
        if let Some(location) = &self.location {
            write!(f, ":{}", location.display())?;
        }
        Ok(())
    }
}

impl Serialize for FrontendId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrontendResult {
    pub id: FrontendId,
    pub state: RuntimeState,
    pub changed: bool,
    pub fix: Option<String>,
    pub detail: Option<String>,
}

fn resolve_id(
    workspace: &Workspace,
    filesystem: FrontendFilesystem,
    environment: FrontendEnvironment,
    location: Option<PathBuf>,
) -> Result<FrontendId> {
    match environment {
        FrontendEnvironment::Host => {
            if filesystem == FrontendFilesystem::Fuse && !cfg!(target_os = "linux") {
                bail!("a host fuse frontend requires a Linux host");
            }
            if filesystem == FrontendFilesystem::Nfs && !cfg!(target_os = "macos") {
                bail!("a host nfs frontend requires a macOS host");
            }
            let location = location.unwrap_or_else(|| {
                resolve_mount_point()
                    .unwrap_or_else(|| workspace.layout().config_dir.join("omnifs"))
            });
            ensure!(
                location.is_absolute(),
                "host frontend location must be absolute: {}",
                location.display()
            );
            Ok(FrontendId::new(filesystem, environment, Some(location)))
        },
        FrontendEnvironment::Docker | FrontendEnvironment::Krunkit => {
            if environment == FrontendEnvironment::Krunkit {
                ensure!(
                    cfg!(target_os = "macos"),
                    "a krunkit frontend requires a macOS host"
                );
            }
            ensure!(
                filesystem == FrontendFilesystem::Fuse,
                "the {environment} environment only delivers a fuse frontend"
            );
            ensure!(
                location.is_none(),
                "the {environment} environment owns its mount; location is not allowed"
            );
            Ok(FrontendId::new(filesystem, environment, None))
        },
    }
}

fn observed_id(frontend: &FrontendStatus) -> FrontendId {
    FrontendId::new(
        frontend.filesystem,
        frontend.environment,
        (frontend.environment == FrontendEnvironment::Host)
            .then(|| frontend.location.clone())
            .flatten(),
    )
}

fn matches(frontend: &FrontendStatus, id: &FrontendId) -> bool {
    observed_id(frontend) == *id
}

fn restart_fix(id: &FrontendId) -> String {
    let mut fix = format!(
        "omnifs frontend restart {} --environment {}",
        id.filesystem(),
        id.environment()
    );
    if let Some(location) = id.location() {
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

fn stopped(id: FrontendId, changed: bool) -> FrontendResult {
    FrontendResult {
        id,
        state: RuntimeState::Stopped,
        changed,
        fix: None,
        detail: None,
    }
}

fn stopped_for_daemon(id: FrontendId) -> FrontendResult {
    FrontendResult {
        id,
        state: RuntimeState::Stopped,
        changed: false,
        fix: Some("omnifs up".into()),
        detail: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnableAction {
    Stopped,
    Attached,
    Reconnect,
    Launch,
}

fn enable_action(
    daemon_running: bool,
    observed: Option<FrontendState>,
    runner_running: bool,
) -> EnableAction {
    if !daemon_running {
        EnableAction::Stopped
    } else if observed == Some(FrontendState::Attached) {
        EnableAction::Attached
    } else if observed == Some(FrontendState::Running) || runner_running {
        EnableAction::Reconnect
    } else {
        EnableAction::Launch
    }
}

fn failed(
    id: FrontendId,
    changed: bool,
    fix: String,
    error: impl std::fmt::Display,
) -> FrontendResult {
    FrontendResult {
        id,
        state: RuntimeState::Failed,
        changed,
        fix: Some(fix),
        detail: Some(error.to_string()),
    }
}

impl FrontendEnableArgs {
    pub async fn enable(self, workspace: &Workspace, output: Output) -> Result<FrontendResult> {
        let id = resolve_id(workspace, self.filesystem, self.environment, self.location)?;
        let inventory = Inventory::collect(workspace).await?;
        if id.environment() == FrontendEnvironment::Host
            && inventory.frontends.iter().any(|row| {
                row.environment == FrontendEnvironment::Host
                    && row.location == id.location().map(Path::to_path_buf)
                    && row.filesystem != id.filesystem()
            })
        {
            bail!(
                "a host frontend is already observed at {} with a different filesystem",
                id.location().unwrap().display()
            );
        }
        if inventory.daemon.status.is_none() {
            return Ok(stopped_for_daemon(id));
        }
        let observed = inventory
            .frontends
            .iter()
            .find(|row| matches(row, &id))
            .map(|row| row.state);
        match enable_action(true, observed, false) {
            EnableAction::Attached => {
                return Ok(FrontendResult {
                    id,
                    state: RuntimeState::Attached,
                    changed: false,
                    fix: None,
                    detail: None,
                });
            },
            EnableAction::Reconnect => return Ok(reconnect_result(workspace, id).await),
            EnableAction::Stopped => unreachable!("daemon state checked above"),
            EnableAction::Launch => {},
        }
        let runner_running = match backend_running(workspace, &id, output).await {
            Ok(running) => running,
            Err(error) => {
                let fix = restart_fix(&id);
                return Ok(failed(id, false, fix, error));
            },
        };
        match enable_action(true, observed, runner_running) {
            EnableAction::Attached => {
                return Ok(FrontendResult {
                    id,
                    state: RuntimeState::Attached,
                    changed: false,
                    fix: None,
                    detail: None,
                });
            },
            EnableAction::Reconnect => return Ok(reconnect_result(workspace, id).await),
            EnableAction::Stopped => unreachable!("daemon state checked above"),
            EnableAction::Launch => {},
        }
        let mount = inventory.mounts.first().map(|mount| mount.name.as_str());
        match launch(workspace, &id, mount, output).await {
            Ok(()) => Ok(wait_attached_result(workspace, id).await),
            Err(error) => {
                let fix = restart_fix(&id);
                Ok(failed(id, true, fix, error))
            },
        }
    }

    pub async fn run(self, output: Output) -> Result<crate::error::ExitCode> {
        let workspace = Workspace::resolve()?;
        let result = self.enable(&workspace, output).await?;
        let inventory = Inventory::collect(&workspace).await?;
        finish_receipt(
            output,
            &FrontendReceipt::from_inventory(&inventory, vec![result]),
        )
    }
}

impl FrontendDisableArgs {
    pub async fn disable(self, workspace: &Workspace, output: Output) -> Result<FrontendResult> {
        if self.environment != FrontendEnvironment::Host && self.location.is_some() {
            bail!(
                "the {} environment owns its mount; location is not allowed",
                self.environment
            );
        }
        let inventory = Inventory::collect(workspace).await?;
        let id = select_disable_id(
            workspace,
            &inventory,
            self.filesystem,
            self.environment,
            self.location,
        )?;
        let observed = inventory.frontends.iter().any(|row| matches(row, &id));
        let running = match backend_running(workspace, &id, output).await {
            Ok(value) => value,
            Err(error) => {
                let fix = disable_fix(&id);
                return Ok(failed(id.clone(), false, fix, error));
            },
        };
        if !running && (!observed || id.environment() != FrontendEnvironment::Host) {
            return Ok(stopped(id, false));
        }
        match stop(workspace, &id, output).await {
            Ok(()) => Ok(stopped(id, true)),
            Err(error) => {
                let fix = disable_fix(&id);
                Ok(failed(id.clone(), false, fix, error))
            },
        }
    }

    pub async fn run(self, output: Output) -> Result<crate::error::ExitCode> {
        let workspace = Workspace::resolve()?;
        let result = self.disable(&workspace, output).await?;
        let inventory = Inventory::collect(&workspace).await?;
        finish_receipt(
            output,
            &FrontendReceipt::from_inventory(&inventory, vec![result]),
        )
    }
}

impl FrontendRestartArgs {
    pub async fn restart(
        self,
        workspace: &Workspace,
        output: Output,
    ) -> Result<Vec<FrontendResult>> {
        if matches!(
            self.environment,
            Some(FrontendEnvironment::Docker | FrontendEnvironment::Krunkit)
        ) && self.location.is_some()
        {
            bail!("guest frontend environments own their mount; location is not allowed");
        }
        let inventory = Inventory::collect(workspace).await?;
        let no_selector =
            self.filesystem.is_none() && self.environment.is_none() && self.location.is_none();
        let targets = if no_selector {
            inventory
                .frontends
                .iter()
                .filter(|row| matches!(row.state, FrontendState::Attached | FrontendState::Running))
                .map(observed_id)
                .collect::<Vec<_>>()
        } else {
            vec![resolve_observed_selector(
                &inventory.frontends,
                self.filesystem,
                self.environment,
                self.location.as_deref(),
            )?]
        };
        if targets.is_empty() {
            bail!("no frontend matches the selector");
        }
        if inventory.daemon.status.is_none() {
            return Ok(targets.into_iter().map(stopped_for_daemon).collect());
        }
        let mut results = Vec::with_capacity(targets.len());
        for id in targets {
            match backend_running(workspace, &id, output).await {
                Ok(true) => {
                    if let Err(error) = stop(workspace, &id, output).await {
                        let fix = restart_fix(&id);
                        results.push(failed(id, false, fix, error));
                        continue;
                    }
                },
                Ok(false) if id.environment() == FrontendEnvironment::Host => {
                    if let Err(error) = stop(workspace, &id, output).await {
                        let fix = restart_fix(&id);
                        results.push(failed(id, false, fix, error));
                        continue;
                    }
                },
                Ok(false) => {},
                Err(error) => {
                    let fix = restart_fix(&id);
                    results.push(failed(id, false, fix, error));
                    continue;
                },
            }
            let fix = restart_fix(&id);
            results.push(
                match launch(
                    workspace,
                    &id,
                    inventory.mounts.first().map(|mount| mount.name.as_str()),
                    output,
                )
                .await
                {
                    Ok(()) => wait_attached_result(workspace, id).await,
                    Err(error) => failed(id, true, fix, error),
                },
            );
        }
        Ok(results)
    }

    pub async fn run(self, output: Output) -> Result<crate::error::ExitCode> {
        let workspace = Workspace::resolve()?;
        let results = self.restart(&workspace, output).await?;
        let inventory = Inventory::collect(&workspace).await?;
        finish_receipt(
            output,
            &FrontendReceipt::from_inventory(&inventory, results),
        )
    }
}

fn select_disable_id(
    workspace: &Workspace,
    inventory: &Inventory,
    filesystem: FrontendFilesystem,
    environment: FrontendEnvironment,
    location: Option<PathBuf>,
) -> Result<FrontendId> {
    let exact = match environment {
        FrontendEnvironment::Host => location.as_deref(),
        _ => None,
    };
    let rows = inventory
        .frontends
        .iter()
        .filter(|row| {
            row.filesystem == filesystem
                && row.environment == environment
                && exact.is_none_or(|path| row.location.as_deref() == Some(path))
        })
        .collect::<Vec<_>>();
    match rows.as_slice() {
        [row] => Ok(observed_id(row)),
        [_, _, ..] => bail!(
            "selector matches multiple frontends: {}",
            rows.iter()
                .map(|row| observed_id(row).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        [] if environment != FrontendEnvironment::Host || location.is_some() => {
            resolve_id(workspace, filesystem, environment, location)
        },
        [] => bail!("no frontend matches the selector"),
    }
}

fn resolve_observed_selector(
    rows: &[FrontendStatus],
    filesystem: Option<FrontendFilesystem>,
    environment: Option<FrontendEnvironment>,
    location: Option<&Path>,
) -> Result<FrontendId> {
    let ids = rows
        .iter()
        .filter(|row| {
            filesystem.is_none_or(|fs| row.filesystem == fs)
                && environment.is_none_or(|env| row.environment == env)
                && location.is_none_or(|path| row.location.as_deref() == Some(path))
        })
        .map(observed_id)
        .collect::<Vec<_>>();
    match ids.as_slice() {
        [] => bail!("no frontend matches the selector"),
        [id] => Ok(id.clone()),
        _ => bail!(
            "selector matches multiple frontends: {}",
            ids.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

async fn reconnect_result(workspace: &Workspace, id: FrontendId) -> FrontendResult {
    let deadline = tokio::time::Instant::now() + RECONNECT_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(inventory) = Inventory::collect(workspace).await
            && inventory
                .frontends
                .iter()
                .any(|row| matches(row, &id) && row.state == FrontendState::Attached)
        {
            return FrontendResult {
                id,
                state: RuntimeState::Attached,
                changed: false,
                fix: None,
                detail: None,
            };
        }
        tokio::time::sleep(POLL).await;
    }
    let fix = restart_fix(&id);
    failed(
        id,
        false,
        fix,
        "frontend process is still running but did not reconnect to the daemon",
    )
}

async fn wait_attached_result(workspace: &Workspace, id: FrontendId) -> FrontendResult {
    let deadline = tokio::time::Instant::now() + RECONNECT_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if let Ok(inventory) = Inventory::collect(workspace).await
            && inventory
                .frontends
                .iter()
                .any(|row| matches(row, &id) && row.state == FrontendState::Attached)
        {
            return FrontendResult {
                id,
                state: RuntimeState::Attached,
                changed: true,
                fix: None,
                detail: None,
            };
        }
        tokio::time::sleep(POLL).await;
    }
    let fix = restart_fix(&id);
    failed(
        id,
        true,
        fix,
        "frontend launched but did not attach to the daemon",
    )
}

async fn backend_running(workspace: &Workspace, id: &FrontendId, output: Output) -> Result<bool> {
    match id.environment() {
        FrontendEnvironment::Host => {
            let location = id.location().context("host frontend has no location")?;
            let state_dir = workspace.layout().frontend_state_dir(
                match id.filesystem() {
                    FrontendFilesystem::Fuse => FrontendKind::Fuse,
                    FrontendFilesystem::Nfs => FrontendKind::Nfs,
                },
                location,
            );
            if !state_dir.try_exists()? {
                return Ok(false);
            }
            let state = MountState::read_unique(&state_dir)?;
            let kind_matches = matches!(
                (id.filesystem(), &state.kind),
                (FrontendFilesystem::Fuse, MountKind::Fuse)
                    | (FrontendFilesystem::Nfs, MountKind::Nfs { .. })
            );
            Ok(kind_matches
                && state.mount_point == location
                && crate::host_teardown::local_mount_is_owned(&state))
        },
        FrontendEnvironment::Docker => {
            let config = workspace.config()?;
            let image = resolve_frontend_image(None, &config)?;
            let name = frontend_container_name(workspace.layout())?;
            let target = DockerTarget::new(name.as_str().to_owned(), image.as_str().to_owned())?;
            Ok(DockerBackend::new(Runtime::connect_for(&target, output)?)
                .is_running()
                .await?
                .unwrap_or(false))
        },
        FrontendEnvironment::Krunkit => {
            Ok(KrunkitBackend::new(workspace.layout().config_dir.clone())
                .is_running()
                .await?
                .unwrap_or(false))
        },
    }
}

async fn launch(
    workspace: &Workspace,
    id: &FrontendId,
    mount: Option<&str>,
    output: Output,
) -> Result<()> {
    let paths = workspace.layout().clone();
    match id.environment() {
        FrontendEnvironment::Host => {
            LocalBackend::new(
                paths.clone(),
                id.location()
                    .context("host frontend has no location")?
                    .to_path_buf(),
                match id.filesystem() {
                    FrontendFilesystem::Fuse => FrontendKind::Fuse,
                    FrontendFilesystem::Nfs => FrontendKind::Nfs,
                }
                .into(),
            )?
            .launch(mount)
            .await
        },
        FrontendEnvironment::Docker => launch_docker(workspace, &paths, mount, output).await,
        FrontendEnvironment::Krunkit => launch_krunkit(workspace, &paths, mount, output).await,
    }
}

async fn launch_docker(
    workspace: &Workspace,
    paths: &WorkspaceLayout,
    mount: Option<&str>,
    output: Output,
) -> Result<()> {
    let config = workspace.config()?;
    let image = resolve_frontend_image(None, &config)?;
    let name = frontend_container_name(paths)?;
    let target = DockerTarget::new(name.as_str().to_owned(), image.as_str().to_owned())?;
    let runtime = Runtime::connect_ready(&target, "omnifs frontend enable", output).await?;
    #[cfg(target_os = "linux")]
    let (bind_ip, expected) = {
        let ip = runtime.frontend_attach_bind_ip().await?;
        (Some(ip), ip)
    };
    #[cfg(not(target_os = "linux"))]
    let (bind_ip, expected) = (None, std::net::Ipv4Addr::LOCALHOST);
    let attach = workspace.daemon().frontend_attach_target(bind_ip).await?;
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

async fn launch_krunkit(
    workspace: &Workspace,
    paths: &WorkspaceLayout,
    mount: Option<&str>,
    output: Output,
) -> Result<()> {
    let config = workspace.config()?;
    let image = GuestImageSource::resolve(None, &config)?
        .into_local_path(&paths.cache_dir, output)
        .await?;
    let attach = workspace.daemon().frontend_attach_target_vsock().await?;
    let backend = KrunkitBackend::new(paths.config_dir.clone());
    backend
        .launch(Path::new(&attach.socket_path), &attach.token, image)
        .await?;
    wait_for_mount(&backend, mount, KRUNKIT_TIMEOUT).await
}

async fn stop(workspace: &Workspace, id: &FrontendId, output: Output) -> Result<()> {
    match id.environment() {
        FrontendEnvironment::Host => crate::host_teardown::teardown_local_frontend(
            &workspace.layout().frontend_state_root(),
            id.location().context("host frontend has no location")?,
            id.filesystem() == FrontendFilesystem::Nfs,
        ),
        FrontendEnvironment::Docker => {
            let config = workspace.config()?;
            let image = resolve_frontend_image(None, &config)?;
            let name = frontend_container_name(workspace.layout())?;
            let target = DockerTarget::new(name.as_str().to_owned(), image.as_str().to_owned())?;
            DockerBackend::new(Runtime::connect_for(&target, output)?)
                .tear_down()
                .await
        },
        FrontendEnvironment::Krunkit => {
            KrunkitBackend::new(workspace.layout().config_dir.clone())
                .tear_down()
                .await
        },
    }
}

async fn wait_for_mount(
    backend: &impl FrontendBackend,
    mount: Option<&str>,
    timeout: Duration,
) -> Result<()> {
    let path = mount.map_or_else(
        || GUEST_MOUNT.to_owned(),
        |name| format!("{GUEST_MOUNT}/{name}"),
    );
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
            return match backend.tear_down().await {
                Ok(()) => Err(anyhow::anyhow!(message)),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "{message}; frontend cleanup also failed: {cleanup:#}"
                )),
            };
        }
        tokio::time::sleep(POLL).await;
    }
}

impl FrontendLsArgs {
    pub async fn run(self, output: Output) -> Result<crate::error::ExitCode> {
        let ws = Workspace::resolve()?;
        let inventory = Inventory::collect(&ws).await?;
        let exit = if inventory.verdict() == crate::inventory::Verdict::Degraded {
            crate::error::ExitCode::Degraded
        } else {
            crate::error::ExitCode::Success
        };
        if output.is_structured() {
            #[derive(Serialize)]
            struct FrontendList {
                frontends: Vec<FrontendStatus>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn selectors_require_one_observed_identity() {
        let rows = vec![
            FrontendStatus {
                filesystem: FrontendFilesystem::Nfs,
                environment: FrontendEnvironment::Host,
                location: Some("/a".into()),
                state: FrontendState::Attached,
                scope: "all",
                mount_count: 0,
                fix: None,
            },
            FrontendStatus {
                filesystem: FrontendFilesystem::Nfs,
                environment: FrontendEnvironment::Host,
                location: Some("/b".into()),
                state: FrontendState::Attached,
                scope: "all",
                mount_count: 0,
                fix: None,
            },
        ];
        assert!(
            resolve_observed_selector(
                &rows,
                Some(FrontendFilesystem::Nfs),
                Some(FrontendEnvironment::Host),
                None
            )
            .is_err()
        );
        let selected = resolve_observed_selector(
            &rows,
            Some(FrontendFilesystem::Nfs),
            Some(FrontendEnvironment::Host),
            Some(Path::new("/b")),
        )
        .unwrap();
        assert_eq!(selected.location(), Some(Path::new("/b")));
    }

    #[test]
    fn enable_decision_covers_runtime_states() {
        assert_eq!(enable_action(false, None, false), EnableAction::Stopped);
        assert_eq!(
            enable_action(true, Some(FrontendState::Attached), true),
            EnableAction::Attached
        );
        assert_eq!(
            enable_action(true, Some(FrontendState::Running), true),
            EnableAction::Reconnect
        );
        assert_eq!(enable_action(true, None, true), EnableAction::Reconnect);
        assert_eq!(enable_action(true, None, false), EnableAction::Launch);
    }

    #[tokio::test]
    async fn mount_probe_cleanup_preserves_error() {
        struct Probe {
            count: Arc<AtomicUsize>,
        }
        impl FrontendBackend for Probe {
            async fn mount_ready(&self, _: &str) -> Result<bool> {
                bail!("probe failed")
            }
            async fn is_running(&self) -> Result<Option<bool>> {
                Ok(Some(true))
            }
            async fn tear_down(&self) -> Result<()> {
                self.count.fetch_add(1, Ordering::SeqCst);
                bail!("cleanup failed")
            }
            fn shell_command(&self, _: Option<&str>, _: &[String]) -> Command {
                Command::new("true")
            }
        }
        let backend = Probe {
            count: Arc::new(AtomicUsize::new(0)),
        };
        let error = wait_for_mount(&backend, None, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(backend.count.load(Ordering::SeqCst), 1);
        assert!(error.to_string().contains("cleanup also failed"));
    }
}
