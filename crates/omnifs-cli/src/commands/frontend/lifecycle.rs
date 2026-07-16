//! Imperative lifecycle for independent frontend runners.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use clap::{Args, ValueEnum};
use omnifs_mtab::{MountKind, MountState, StateError};
use omnifs_workspace::daemon_record::FrontendKind;
use omnifs_workspace::layout::{WorkspaceLayout, resolve_mount_point};
use serde::{Deserialize, Serialize};

use crate::commands::frontend::GUEST_MOUNT;
use crate::commands::receipt::FrontendReceipt;
use crate::docker::{DockerClient, DockerRunner, DockerTarget};
use crate::frontend_container::{frontend_container_name, resolve_frontend_image};
use crate::host_runner::{HostRunner, LocalProtocol};
use crate::inventory::{FrontendState, FrontendStatus, Inventory};
use crate::libkrun_runner::{LibkrunLaunchRequest, LibkrunRunner};
use crate::ui::output::{Output, ResultVerdict};
use crate::workspace::Workspace;

const DOCKER_TIMEOUT: Duration = Duration::from_secs(5);
const LIBKRUN_TIMEOUT: Duration = Duration::from_secs(90);
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
    const ALL: [Self; 2] = [Self::Fuse, Self::Nfs];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Fuse => "fuse",
            Self::Nfs => "nfs",
        }
    }

    const fn default_runtime(self) -> FrontendRuntime {
        match self {
            Self::Fuse if cfg!(target_os = "macos") => FrontendRuntime::Libkrun,
            Self::Fuse | Self::Nfs => FrontendRuntime::Host,
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
pub enum FrontendRuntime {
    Host,
    Docker,
    Libkrun,
}

impl FrontendRuntime {
    const ALL: [Self; 3] = [Self::Host, Self::Docker, Self::Libkrun];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::Libkrun => "libkrun",
        }
    }

    fn supports(self, filesystem: FrontendFilesystem) -> bool {
        matches!(
            (std::env::consts::OS, filesystem, self),
            (
                "macos",
                FrontendFilesystem::Fuse,
                Self::Docker | Self::Libkrun
            ) | ("macos", FrontendFilesystem::Nfs, Self::Host)
                | ("linux", FrontendFilesystem::Fuse, Self::Host | Self::Docker)
        )
    }

    const fn instances(self) -> InstancePolicy {
        match self {
            Self::Host => InstancePolicy::MultipleLocations,
            Self::Docker | Self::Libkrun => InstancePolicy::OnePerWorkspace,
        }
    }
}

impl std::fmt::Display for FrontendRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct FrontendId {
    filesystem: FrontendFilesystem,
    runtime: FrontendRuntime,
    location: Option<PathBuf>,
}

impl FrontendId {
    pub fn new(
        filesystem: FrontendFilesystem,
        runtime: FrontendRuntime,
        location: Option<PathBuf>,
    ) -> Self {
        Self {
            filesystem,
            runtime,
            location,
        }
    }
    pub const fn filesystem(&self) -> FrontendFilesystem {
        self.filesystem
    }
    pub const fn runtime(&self) -> FrontendRuntime {
        self.runtime
    }
    pub fn location(&self) -> Option<&Path> {
        self.location.as_deref()
    }
}

impl std::fmt::Display for FrontendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.filesystem, self.runtime)?;
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
    /// Runner environment. Defaults to libkrun for FUSE on macOS and host otherwise.
    #[arg(long, value_enum)]
    pub runtime: Option<FrontendRuntime>,
    #[arg(long)]
    pub location: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct FrontendDisableArgs {
    #[arg(value_enum)]
    pub filesystem: FrontendFilesystem,
    #[arg(long, value_enum)]
    pub runtime: FrontendRuntime,
    #[arg(long)]
    pub location: Option<PathBuf>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendRestartArgs {
    #[arg(value_enum)]
    pub filesystem: Option<FrontendFilesystem>,
    #[arg(long, value_enum)]
    pub runtime: Option<FrontendRuntime>,
    #[arg(long)]
    pub location: Option<PathBuf>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendLsArgs {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Platform {
    os: &'static str,
    arch: &'static str,
}

impl Platform {
    const fn current() -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }

    fn label(self) -> String {
        let os = match self.os {
            "macos" => "macOS",
            "linux" => "Linux",
            other => other,
        };
        format!("{os} {}", self.arch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum InstancePolicy {
    MultipleLocations,
    OnePerWorkspace,
}

impl InstancePolicy {
    const fn label(self) -> &'static str {
        match self {
            Self::MultipleLocations => "multiple locations",
            Self::OnePerWorkspace => "one per workspace",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct FrontendSupport {
    filesystem: FrontendFilesystem,
    runtime: FrontendRuntime,
    default: bool,
    instances: InstancePolicy,
    available: bool,
    detail: String,
}

impl FrontendSupport {
    async fn inspect(filesystem: FrontendFilesystem, runtime: FrontendRuntime) -> Self {
        let default = filesystem.default_runtime() == runtime;
        let readiness = match runtime {
            FrontendRuntime::Host => HostRunner::probe(match filesystem {
                FrontendFilesystem::Fuse => LocalProtocol::Fuse,
                FrontendFilesystem::Nfs => LocalProtocol::Nfs,
            }),
            FrontendRuntime::Docker => DockerClient::probe().await,
            FrontendRuntime::Libkrun => LibkrunRunner::probe(),
        };
        let command = if default {
            format!("omnifs frontend enable {filesystem}")
        } else {
            format!("omnifs frontend enable {filesystem} --runtime {runtime}")
        };
        let (available, detail) = match readiness {
            Ok(()) => (true, command),
            Err(error) => (false, format!("{error:#}")),
        };
        FrontendSupport {
            filesystem,
            runtime,
            default,
            instances: runtime.instances(),
            available,
            detail,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct FrontendList {
    platform: Platform,
    supported_frontends: Vec<FrontendSupport>,
    frontends: Vec<FrontendStatus>,
    verdict: crate::inventory::Verdict,
}

impl FrontendList {
    async fn collect(inventory: &Inventory) -> Self {
        let platform = Platform::current();
        let mut supported_frontends = Vec::new();
        for filesystem in FrontendFilesystem::ALL {
            for runtime in FrontendRuntime::ALL {
                if runtime.supports(filesystem) {
                    supported_frontends.push(FrontendSupport::inspect(filesystem, runtime).await);
                }
            }
        }
        Self {
            platform,
            supported_frontends,
            frontends: inventory.frontends.clone(),
            verdict: inventory.verdict(),
        }
    }

    fn support_table(&self) -> crate::ui::table::ResourceTable {
        use crate::ui::table::{
            Cell, Column, Priority, ResourceRow, ResourceTable, StateToken, WidthPolicy,
        };

        let mut table = ResourceTable::new(
            format!("Supported frontends on {}", self.platform.label()),
            self.supported_frontends.len(),
            vec![
                Column::new("Filesystem", Priority::Identity, WidthPolicy::Auto),
                Column::new("Runtime", Priority::Identity, WidthPolicy::Auto),
                Column::new("Default", Priority::Detail, WidthPolicy::Auto),
                Column::new("Instances", Priority::Essential, WidthPolicy::Auto),
                Column::new("Availability", Priority::Essential, WidthPolicy::Auto),
                Column::new("Enable or reason", Priority::Essential, WidthPolicy::Path),
            ],
        );
        for support in &self.supported_frontends {
            let state = if support.available {
                StateToken::positive("available")
            } else {
                StateToken::neutral("unavailable")
            };
            table.push(ResourceRow::new(
                [
                    Cell::new(support.filesystem.label()),
                    Cell::new(support.runtime.label()),
                    Cell::new(if support.default { "yes" } else { "no" }),
                    Cell::new(support.instances.label()),
                    Cell::state(state.clone()),
                    Cell::new(&support.detail),
                ],
                state,
            ));
        }
        table
    }

    fn render(&self) -> crate::ui::table::Report {
        use crate::ui::table::{Block, Report};
        let mut report = Report::new();
        report.push(Block::Resources(self.support_table()));
        let mut instantiated = crate::status::frontend_table(&self.frontends);
        "Instantiated frontends".clone_into(&mut instantiated.title);
        report.push(Block::Resources(instantiated));
        report
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendResultState {
    Stopped,
    Attached,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrontendResult {
    pub id: FrontendId,
    pub state: FrontendResultState,
    pub changed: bool,
    pub fix: Option<String>,
    pub detail: Option<String>,
}

fn resolve_id(
    workspace: &Workspace,
    filesystem: FrontendFilesystem,
    runtime: FrontendRuntime,
    location: Option<PathBuf>,
) -> Result<FrontendId> {
    ensure!(
        runtime.supports(filesystem),
        "a {filesystem}/{runtime} frontend is not supported on {}",
        std::env::consts::OS
    );
    match runtime {
        FrontendRuntime::Host => {
            let location = location.unwrap_or_else(|| {
                resolve_mount_point()
                    .unwrap_or_else(|| workspace.layout().config_dir.join("omnifs"))
            });
            ensure!(
                location.is_absolute(),
                "host frontend location must be absolute: {}",
                location.display()
            );
            Ok(FrontendId::new(filesystem, runtime, Some(location)))
        },
        FrontendRuntime::Docker | FrontendRuntime::Libkrun => {
            ensure!(
                location.is_none(),
                "the {runtime} runtime owns its mount; location is not allowed"
            );
            Ok(FrontendId::new(filesystem, runtime, None))
        },
    }
}

fn observed_id(frontend: &FrontendStatus) -> FrontendId {
    FrontendId::new(
        frontend.filesystem,
        frontend.runtime,
        (frontend.runtime == FrontendRuntime::Host)
            .then(|| frontend.location.clone())
            .flatten(),
    )
}

fn matches(frontend: &FrontendStatus, id: &FrontendId) -> bool {
    observed_id(frontend) == *id
}

fn restart_fix(id: &FrontendId) -> String {
    let mut fix = format!(
        "omnifs frontend restart {} --runtime {}",
        id.filesystem(),
        id.runtime()
    );
    if let Some(location) = id.location() {
        fix.push_str(" --location ");
        fix.push_str(&location.to_string_lossy());
    }
    fix
}

fn disable_fix(id: &FrontendId) -> String {
    let mut fix = format!(
        "omnifs frontend disable {} --runtime {}",
        id.filesystem(),
        id.runtime()
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
        state: FrontendResultState::Stopped,
        changed,
        fix: None,
        detail: None,
    }
}

fn stopped_for_daemon(id: FrontendId) -> FrontendResult {
    FrontendResult {
        id,
        state: FrontendResultState::Stopped,
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
        state: FrontendResultState::Failed,
        changed,
        fix: Some(fix),
        detail: Some(error.to_string()),
    }
}

impl FrontendEnableArgs {
    pub async fn enable(self, workspace: &Workspace, output: Output) -> Result<FrontendResult> {
        let runtime = self
            .runtime
            .unwrap_or_else(|| self.filesystem.default_runtime());
        let id = resolve_id(workspace, self.filesystem, runtime, self.location)?;
        let inventory = Inventory::collect(workspace).await?;
        if id.runtime() == FrontendRuntime::Host
            && inventory.frontends.iter().any(|row| {
                row.runtime == FrontendRuntime::Host
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
                    state: FrontendResultState::Attached,
                    changed: false,
                    fix: None,
                    detail: None,
                });
            },
            EnableAction::Reconnect => return Ok(reconnect_result(workspace, id).await),
            EnableAction::Stopped => unreachable!("daemon state checked above"),
            EnableAction::Launch => {},
        }
        let runner_running = match runner_running(workspace, &id, output.clone()).await {
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
                    state: FrontendResultState::Attached,
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
        match launch(workspace, &id, mount, output.clone()).await {
            Ok(()) if id.runtime() == FrontendRuntime::Libkrun => Ok(attached_result(id, true)),
            Ok(()) => Ok(wait_attached_result(workspace, id).await),
            Err(error) => {
                let fix = restart_fix(&id);
                Ok(failed(id, true, fix, error))
            },
        }
    }

    pub async fn run(self, output: Output) -> Result<crate::error::ExitCode> {
        let workspace = Workspace::resolve()?;
        let result = self.enable(&workspace, output.clone()).await?;
        let inventory = Inventory::collect(&workspace).await?;
        finish_receipt(
            &output,
            &FrontendReceipt::from_inventory(&inventory, vec![result]),
        )
    }
}

impl FrontendDisableArgs {
    pub async fn disable(self, workspace: &Workspace, output: Output) -> Result<FrontendResult> {
        if self.runtime != FrontendRuntime::Host && self.location.is_some() {
            bail!(
                "the {} runtime owns its mount; location is not allowed",
                self.runtime
            );
        }
        let inventory = Inventory::collect(workspace).await?;
        let id = select_disable_id(
            workspace,
            &inventory,
            self.filesystem,
            self.runtime,
            self.location,
        )?;
        let observed = inventory.frontends.iter().any(|row| matches(row, &id));
        let running = match runner_running(workspace, &id, output.clone()).await {
            Ok(value) => value,
            Err(error) => {
                let fix = disable_fix(&id);
                return Ok(failed(id.clone(), false, fix, error));
            },
        };
        if !running && (!observed || id.runtime() != FrontendRuntime::Host) {
            return Ok(stopped(id, false));
        }
        match stop(workspace, &id, output.clone()).await {
            Ok(()) => Ok(stopped(id, true)),
            Err(error) => {
                let fix = disable_fix(&id);
                Ok(failed(id.clone(), false, fix, error))
            },
        }
    }

    pub async fn run(self, output: Output) -> Result<crate::error::ExitCode> {
        let workspace = Workspace::resolve()?;
        let result = self.disable(&workspace, output.clone()).await?;
        let inventory = Inventory::collect(&workspace).await?;
        finish_receipt(
            &output,
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
            self.runtime,
            Some(FrontendRuntime::Docker | FrontendRuntime::Libkrun)
        ) && self.location.is_some()
        {
            bail!("guest frontend runtimes own their mount; location is not allowed");
        }
        let inventory = Inventory::collect(workspace).await?;
        let no_selector =
            self.filesystem.is_none() && self.runtime.is_none() && self.location.is_none();
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
                self.runtime,
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
            match runner_running(workspace, &id, output.clone()).await {
                Ok(true) if id.runtime() == FrontendRuntime::Libkrun => {},
                Ok(true) => {
                    if let Err(error) = stop(workspace, &id, output.clone()).await {
                        let fix = restart_fix(&id);
                        results.push(failed(id, false, fix, error));
                        continue;
                    }
                },
                Ok(false) if id.runtime() == FrontendRuntime::Host => {
                    if let Err(error) = stop(workspace, &id, output.clone()).await {
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
                    output.clone(),
                )
                .await
                {
                    Ok(()) if id.runtime() == FrontendRuntime::Libkrun => attached_result(id, true),
                    Ok(()) => wait_attached_result(workspace, id).await,
                    Err(error) => failed(id, true, fix, error),
                },
            );
        }
        Ok(results)
    }

    pub async fn run(self, output: Output) -> Result<crate::error::ExitCode> {
        let workspace = Workspace::resolve()?;
        let results = self.restart(&workspace, output.clone()).await?;
        let inventory = Inventory::collect(&workspace).await?;
        finish_receipt(
            &output,
            &FrontendReceipt::from_inventory(&inventory, results),
        )
    }
}

fn select_disable_id(
    workspace: &Workspace,
    inventory: &Inventory,
    filesystem: FrontendFilesystem,
    runtime: FrontendRuntime,
    location: Option<PathBuf>,
) -> Result<FrontendId> {
    let exact = match runtime {
        FrontendRuntime::Host => location.as_deref(),
        _ => None,
    };
    let rows = inventory
        .frontends
        .iter()
        .filter(|row| {
            row.filesystem == filesystem
                && row.runtime == runtime
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
        [] if runtime != FrontendRuntime::Host || location.is_some() => {
            resolve_id(workspace, filesystem, runtime, location)
        },
        [] => bail!("no frontend matches the selector"),
    }
}

fn resolve_observed_selector(
    rows: &[FrontendStatus],
    filesystem: Option<FrontendFilesystem>,
    runtime: Option<FrontendRuntime>,
    location: Option<&Path>,
) -> Result<FrontendId> {
    let ids = rows
        .iter()
        .filter(|row| {
            filesystem.is_none_or(|fs| row.filesystem == fs)
                && runtime.is_none_or(|env| row.runtime == env)
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
                state: FrontendResultState::Attached,
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
            return attached_result(id, true);
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

fn attached_result(id: FrontendId, changed: bool) -> FrontendResult {
    FrontendResult {
        id,
        state: FrontendResultState::Attached,
        changed,
        fix: None,
        detail: None,
    }
}

async fn runner_running(workspace: &Workspace, id: &FrontendId, output: Output) -> Result<bool> {
    match id.runtime() {
        FrontendRuntime::Host => {
            let location = id.location().context("host frontend has no location")?;
            let state_dir = workspace.layout().frontend_state_dir(
                match id.filesystem() {
                    FrontendFilesystem::Fuse => FrontendKind::Fuse,
                    FrontendFilesystem::Nfs => FrontendKind::Nfs,
                },
                location,
            );
            let states = MountState::read_all(&state_dir)?;
            match states.as_slice() {
                [] => {
                    if omnifs_nfs::mount_is_active_checked(location)? {
                        bail!(
                            "active host frontend mount {} has no unique typed state; refusing to operate",
                            location.display()
                        );
                    }
                    Ok(false)
                },
                [state] => {
                    let kind_matches = matches!(
                        (id.filesystem(), &state.kind),
                        (FrontendFilesystem::Fuse, MountKind::Fuse)
                            | (FrontendFilesystem::Nfs, MountKind::Nfs { .. })
                    );
                    Ok(kind_matches
                        && state.mount_point == location
                        && crate::host_teardown::local_mount_is_owned(state))
                },
                states => Err(StateError::RecordCount(states.len()).into()),
            }
        },
        FrontendRuntime::Docker => {
            let config = workspace.config()?;
            let image = resolve_frontend_image(None, &config)?;
            let name = frontend_container_name(workspace.layout())?;
            let target = DockerTarget::new(name.as_str().to_owned(), image.as_str().to_owned())?;
            Ok(
                DockerRunner::new(DockerClient::connect_for(&target, output)?)
                    .is_running()
                    .await?
                    .unwrap_or(false),
            )
        },
        FrontendRuntime::Libkrun => Ok(LibkrunRunner::new(workspace.layout().config_dir.clone())
            .is_running()?
            .unwrap_or(false)),
    }
}

async fn launch(
    workspace: &Workspace,
    id: &FrontendId,
    mount: Option<&str>,
    output: Output,
) -> Result<()> {
    let paths = workspace.layout().clone();
    match id.runtime() {
        FrontendRuntime::Host => {
            HostRunner::new(
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
        FrontendRuntime::Docker => launch_docker(workspace, &paths, mount, output).await,
        FrontendRuntime::Libkrun => launch_libkrun(workspace, &paths, id, mount, output).await,
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
    let runtime = DockerClient::connect_ready(&target, "omnifs frontend enable", output).await?;
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
    let runner = DockerRunner::new(runtime);
    runner
        .launch(&paths.config_dir, addr.port(), &attach.token)
        .await?;
    wait_for_mount(&runner, mount, DOCKER_TIMEOUT).await
}

async fn launch_libkrun(
    workspace: &Workspace,
    paths: &WorkspaceLayout,
    id: &FrontendId,
    mount: Option<&str>,
    output: Output,
) -> Result<()> {
    let config = workspace.config()?;
    let attach = workspace.daemon().frontend_attach_target_vsock().await?;
    let runner = LibkrunRunner::new(paths.config_dir.clone());
    let attached = async {
        let result = wait_attached_result(workspace, id.clone()).await;
        if result.state == FrontendResultState::Attached {
            Ok(())
        } else {
            bail!(
                "{}",
                result
                    .detail
                    .unwrap_or_else(|| "frontend launched but did not attach to the daemon".into())
            )
        }
    };
    runner
        .launch(
            LibkrunLaunchRequest {
                daemon_attach_socket: Path::new(&attach.socket_path),
                attach_token: &attach.token,
                config: &config,
                cache_dir: &paths.cache_dir,
                output,
                mount,
                timeout: LIBKRUN_TIMEOUT,
            },
            attached,
        )
        .await
}

async fn stop(workspace: &Workspace, id: &FrontendId, output: Output) -> Result<()> {
    match id.runtime() {
        FrontendRuntime::Host => crate::host_teardown::teardown_local_frontend(
            &workspace.layout().frontend_state_root(),
            id.location().context("host frontend has no location")?,
            id.filesystem() == FrontendFilesystem::Nfs,
        ),
        FrontendRuntime::Docker => {
            let config = workspace.config()?;
            let image = resolve_frontend_image(None, &config)?;
            let name = frontend_container_name(workspace.layout())?;
            let target = DockerTarget::new(name.as_str().to_owned(), image.as_str().to_owned())?;
            DockerRunner::new(DockerClient::connect_for(&target, output)?)
                .tear_down()
                .await
        },
        FrontendRuntime::Libkrun => {
            LibkrunRunner::new(workspace.layout().config_dir.clone())
                .tear_down()
                .await
        },
    }
}

async fn wait_for_mount(
    runner: &DockerRunner,
    mount: Option<&str>,
    timeout: Duration,
) -> Result<()> {
    let path = mount.map_or_else(
        || GUEST_MOUNT.to_owned(),
        |name| format!("{GUEST_MOUNT}/{name}"),
    );
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match runner.mount_ready(&path).await {
            Ok(true) => return Ok(()),
            Ok(false) => {},
            Err(error) => {
                let cleanup = runner.tear_down().await.err();
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
            return match runner.tear_down().await {
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
        let workspace = Workspace::resolve()?;
        let inventory = Inventory::collect(&workspace).await?;
        let list = FrontendList::collect(&inventory).await;
        let exit = if inventory.verdict() == crate::inventory::Verdict::Degraded {
            crate::error::ExitCode::Degraded
        } else {
            crate::error::ExitCode::Success
        };
        if output.is_structured() {
            output.emit_result(ResultVerdict::from(inventory.verdict()), &list)?;
        } else {
            list.render().print();
        }
        Ok(exit)
    }
}

fn finish_receipt(output: &Output, receipt: &FrontendReceipt) -> Result<crate::error::ExitCode> {
    if output.is_structured() {
        output.emit_result(receipt.output_verdict(), receipt)?;
    } else {
        render_frontend_receipt(receipt);
        for result in receipt
            .rows
            .iter()
            .filter(|result| result.state == FrontendResultState::Failed)
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
                "Runtime",
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
                crate::ui::table::Cell::new(path.runtime.label()),
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

    #[test]
    fn frontend_defaults_follow_the_platform_surface() {
        assert_eq!(
            FrontendFilesystem::Nfs.default_runtime(),
            FrontendRuntime::Host
        );
        assert_eq!(
            FrontendFilesystem::Fuse.default_runtime(),
            if cfg!(target_os = "macos") {
                FrontendRuntime::Libkrun
            } else {
                FrontendRuntime::Host
            }
        );
    }

    #[test]
    fn selectors_require_one_observed_identity() {
        let rows = vec![
            FrontendStatus {
                filesystem: FrontendFilesystem::Nfs,
                runtime: FrontendRuntime::Host,
                location: Some("/a".into()),
                state: FrontendState::Attached,
                scope: "all",
                mount_count: 0,
                fix: None,
            },
            FrontendStatus {
                filesystem: FrontendFilesystem::Nfs,
                runtime: FrontendRuntime::Host,
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
                Some(FrontendRuntime::Host),
                None
            )
            .is_err()
        );
        let selected = resolve_observed_selector(
            &rows,
            Some(FrontendFilesystem::Nfs),
            Some(FrontendRuntime::Host),
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
}
