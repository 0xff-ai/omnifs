//! Shared launch choreography for `omnifs up`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use omnifs_api::{API_MAJOR, API_MINOR, DaemonStatus, DaemonSubsystem, FsType};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::mounts::materialize::{self, MaterializationMode, MaterializedMount};
use omnifs_workspace::runtime_record::{
    Endpoint, FrontendKind as RecordFrontendKind, FrontendRecord, RecordedBackend, RuntimeRecord,
};

use crate::client::{DaemonClient, env_daemon_addr};
use crate::config::ConfiguredBackend;
use crate::launch_backend::{BackendOverrides, DockerTarget, LaunchBackend};
use crate::mount_config::MountConfig;
use crate::runtime::Runtime;
use crate::workspace::Workspace;
use omnifs_workspace::provider::Catalog;

/// Command-owned daemon launcher.
///
/// `Launcher` is the policy boundary for `omnifs up`: setup state, backend
/// resolution, mount discovery, provider bundle installation, contract
/// preflight, runtime launch, and user-facing next steps stay behind this
/// interface instead of being reassembled by the command.
pub(crate) struct Launcher<'a> {
    workspace: &'a Workspace,
    verb: &'static str,
    /// Per-launch runtime override from `omnifs up --runtime`. When set it wins
    /// over the persisted `[system].runtime` and is never written back.
    runtime_override: Option<ConfiguredBackend>,
}

impl<'a> Launcher<'a> {
    pub(crate) fn new(workspace: &'a Workspace, verb: &'static str) -> Self {
        Self {
            workspace,
            verb,
            runtime_override: None,
        }
    }

    /// Override the runtime for this launch only (not persisted to config).
    pub(crate) fn with_runtime_override(mut self, runtime: Option<ConfiguredBackend>) -> Self {
        self.runtime_override = runtime;
        self
    }

    pub(crate) async fn launch(self) -> anyhow::Result<LaunchOutcome> {
        let paths = self.workspace.layout();
        let config = self.workspace.config()?;
        let telemetry_enabled = config.telemetry_enabled();
        // An explicit `--runtime` chooses the backend for this launch and skips
        // the setup gate; otherwise the persisted default decides, and a missing
        // one means setup never ran.
        let backend = if self.runtime_override.is_none() && config.system.runtime.is_none() {
            anyhow::bail!(
                "`{}` requires setup to choose a daemon backend; run `omnifs setup` first, or pass `--runtime <docker|native>`",
                self.verb
            );
        } else {
            LaunchBackend::resolve(
                BackendOverrides {
                    runtime: self.runtime_override,
                },
                &config,
            )?
        };

        let configs = self.workspace.mounts()?;
        if configs.is_empty() {
            anyhow::bail!(
                "no mount configs found in {}; run `omnifs setup` for guided onboarding, or `omnifs init <provider>` to add one directly",
                paths.mounts_dir.display()
            );
        }

        crate::provider_bundle::ensure_providers_installed(&paths.providers_dir)?;

        crate::upgrade::run_upgrade_check(&paths.providers_dir, &configs)?;

        anstream::eprintln!("Using mount configs from {}", paths.mounts_dir.display());
        launch_runtime(
            LaunchSpec {
                backend,
                paths,
                store: Box::new(FileStore::new(&paths.credentials_file)),
                verb: self.verb,
                configs,
                extra_binds: Vec::new(),
                extra_env: Vec::new(),
                reuse_existing_container: true,
                telemetry_enabled,
            },
            self.workspace.catalog(),
        )
        .await
    }
}

#[derive(Debug, Clone)]
pub(crate) enum LaunchOutcome {
    Native { mount_point: Option<PathBuf> },
    Docker { target: DockerTarget },
}

/// Everything a caller must supply to run the full launch sequence.
pub(crate) struct LaunchSpec<'a> {
    pub backend: LaunchBackend,
    pub paths: &'a WorkspaceLayout,
    pub store: Box<dyn CredentialStore>,
    /// Command name shown in Docker-readiness diagnostics, e.g. `"omnifs up"`.
    pub verb: &'static str,
    /// Mount configs to materialize and push to the daemon.
    pub configs: Vec<MountConfig>,
    /// Extra binds layered on top of materialized preopens.
    pub extra_binds: Vec<String>,
    /// Extra environment variables for the runtime container.
    pub extra_env: Vec<String>,
    /// Whether a same-image running container may be reused.
    pub reuse_existing_container: bool,
    /// Effective telemetry state, propagated to the launched daemon so the
    /// CLI's `[telemetry] enabled = false` off-switch reaches it (the daemon
    /// has no strict-config channel and reads `OMNIFS_TELEMETRY`).
    pub telemetry_enabled: bool,
}

/// Docker-specific mount spec builder for launch-time container binds.
///
/// The daemon still reads specs from `mounts/` and reconciles them itself. This
/// builder exists only for the container invariant Docker imposes: host
/// preopen directories must be known before `docker create`, while credential
/// failures should still surface before the daemon starts.
pub(crate) struct DockerMountSpecBuilder<'a> {
    catalog: &'a Catalog,
    store: &'a dyn CredentialStore,
}

impl<'a> DockerMountSpecBuilder<'a> {
    pub(crate) fn new(catalog: &'a Catalog, store: &'a dyn CredentialStore) -> Self {
        Self { catalog, store }
    }

    pub(crate) fn materialize(&self, config: &MountConfig) -> anyhow::Result<MaterializedMount> {
        let materialized = materialize::materialize(
            config.config.clone(),
            self.catalog,
            MaterializationMode::Docker,
        )
        .with_context(|| format!("materialize mount {}", config.source.display()))?;

        let mount_auth =
            crate::auth::MountAuth::from_spec(self.catalog, materialized.spec().clone());
        config.validate_host_managed_credentials(&mount_auth, self.store)?;

        Ok(materialized)
    }

    fn materialize_bind_specs<'configs>(
        &self,
        configs: impl IntoIterator<Item = &'configs MountConfig>,
    ) -> anyhow::Result<Vec<String>> {
        Ok(configs
            .into_iter()
            .map(|config| self.materialize(config))
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .flat_map(|mount| mount.into_preopen_binds().into_docker_bind_specs())
            .collect())
    }
}

/// Run the full materialize → connect → launch → wait → push sequence.
pub(crate) async fn launch_runtime(
    spec: LaunchSpec<'_>,
    catalog: &Catalog,
) -> anyhow::Result<LaunchOutcome> {
    let LaunchSpec {
        backend,
        paths,
        store,
        verb,
        configs,
        extra_binds,
        mut extra_env,
        reuse_existing_container,
        telemetry_enabled,
    } = spec;

    std::fs::create_dir_all(&paths.config_dir)
        .with_context(|| format!("create runtime home {}", paths.config_dir.display()))?;

    let target = match backend {
        LaunchBackend::Native => return launch_host_native(paths, verb, telemetry_enabled).await,
        LaunchBackend::Docker(target) => target,
    };

    // Carry the off-switch into the container's daemon. Only push it when
    // disabled: an unset `OMNIFS_TELEMETRY` reads as enabled.
    if !telemetry_enabled {
        extra_env.push(format!("{}=0", omnifs_workspace::telemetry::ENV_SWITCH));
    }

    // The container daemon serves TCP, so it needs the bearer token. Generate it
    // host-side and inject it by name; the host CLI then dials with the same
    // value, and it is written into the record host-side once the daemon serves.
    let token = generate_control_token();
    extra_env.push(format!("OMNIFS_CONTROL_TOKEN={token}"));

    anstream::eprintln!("Computing container binds for {} mount(s)", configs.len());
    let preopen_binds =
        DockerMountSpecBuilder::new(catalog, store.as_ref()).materialize_bind_specs(&configs)?;
    let all_binds: Vec<String> = preopen_binds.into_iter().chain(extra_binds).collect();

    let rt = Runtime::connect_ready(&target, verb).await?;
    rt.launch_container(
        &paths.config_dir,
        all_binds,
        extra_env,
        reuse_existing_container,
    )
    .await?;

    match finish_docker_launch(&rt, paths, &target, &token).await {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            if let Err(teardown) = rt.remove().await {
                anstream::eprintln!("also failed to remove the container: {teardown:#}");
            }
            Err(error)
        },
    }
}

/// The loopback address the Docker container publishes its control port on,
/// honoring an `OMNIFS_DAEMON_ADDR` override for non-default setups.
fn published_control_addr() -> String {
    env_daemon_addr().unwrap_or_else(|| omnifs_api::default_listen_addr().to_string())
}

/// A fresh 16-byte hex control token. Failure to draw randomness is fatal: a
/// predictable token would weaken the only guard on the TCP control port.
fn generate_control_token() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("draw control token randomness");
    hex::encode(bytes)
}

/// Parse an `OMNIFS_DAEMON_ADDR` override into a `SocketAddr`, erroring on a
/// malformed value rather than silently falling back to a default port.
fn env_control_addr() -> anyhow::Result<Option<SocketAddr>> {
    match env_daemon_addr() {
        None => Ok(None),
        Some(addr) => addr.parse().map(Some).with_context(|| {
            format!("OMNIFS_DAEMON_ADDR is not a valid host:port address: {addr:?}")
        }),
    }
}

/// Spawn a detached host-native daemon and wait for it to serve. The daemon
/// serves its Unix socket and writes the runtime record itself; the CLI reads
/// that record to reach it, triggers one more reconcile to converge any change
/// since start, and surfaces per-mount failures. Only the debug/test path
/// (`OMNIFS_DAEMON_ADDR` set) adds a TCP listener.
async fn launch_host_native(
    paths: &WorkspaceLayout,
    verb: &str,
    telemetry_enabled: bool,
) -> anyhow::Result<LaunchOutcome> {
    reject_existing_host_daemon(paths, verb).await?;
    anstream::eprintln!("Starting omnifs daemon (host-native)");

    let tcp_addr = env_control_addr()?;
    crate::launch_backend::launch_native(paths, tcp_addr, telemetry_enabled).await?;

    let client = DaemonClient::for_layout(paths);
    match client.reconcile().await {
        Ok(report) => report_reconcile_failures(&report),
        Err(error) => {
            return Err(error);
        },
    }

    let status = client.status().await.ok();
    if let Some(status) = &status {
        report_launch_status(status);
    }
    Ok(LaunchOutcome::Native {
        mount_point: status.map(|status| status.mount_point),
    })
}

async fn reject_existing_host_daemon(paths: &WorkspaceLayout, verb: &str) -> anyhow::Result<()> {
    let client = DaemonClient::for_layout(paths);
    let Some(status) = client.status_optional().await? else {
        return Ok(());
    };

    anyhow::bail!("{}", ExistingDaemon::new(status, paths, verb))
}

struct ExistingDaemon {
    status: DaemonStatus,
    paths: WorkspaceLayout,
    verb: String,
}

impl ExistingDaemon {
    fn new(status: DaemonStatus, paths: &WorkspaceLayout, verb: &str) -> Self {
        Self {
            status,
            paths: paths.clone(),
            verb: verb.to_string(),
        }
    }

    fn daemon_executable(&self) -> &Path {
        self.status.executable.as_path()
    }

    fn paths_match(&self) -> bool {
        same_path(&self.status.config_dir, &self.paths.config_dir)
            && same_path(&self.status.cache_dir, &self.paths.cache_dir)
    }

    fn executable_matches(&self) -> Option<bool> {
        let daemon = self.daemon_executable();
        if daemon.as_os_str().is_empty() {
            return None;
        }
        std::env::current_exe()
            .map(|current| same_path(daemon, &current))
            .ok()
    }

    /// True when the running daemon's API major or build version differs from
    /// this CLI's, i.e. an upgrade boundary rather than a duplicate launch.
    fn version_skew(&self) -> bool {
        self.status.api_major != API_MAJOR || self.status.version != env!("CARGO_PKG_VERSION")
    }

    fn title(&self) -> &'static str {
        if self.version_skew() {
            "A different omnifs daemon is already running"
        } else if !self.paths_match() {
            "An omnifs daemon is already running for a different home"
        } else if self.executable_matches() == Some(false) {
            "A different omnifs daemon is already running"
        } else {
            "omnifs daemon is already running"
        }
    }
}

impl std::fmt::Display for ExistingDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.title())?;
        writeln!(
            f,
            "  daemon  v{}  API {}.{}  pid {}  {}",
            self.status.version,
            self.status.api_major,
            self.status.api_minor,
            self.status.pid,
            display_path(self.daemon_executable())
        )?;
        writeln!(
            f,
            "  this    v{}  API {}.{}       {}",
            env!("CARGO_PKG_VERSION"),
            API_MAJOR,
            API_MINOR,
            display_path(&std::env::current_exe().unwrap_or_else(|_| PathBuf::new()))
        )?;
        writeln!(f)?;
        writeln!(f, "  daemon config  {}", self.status.config_dir.display())?;
        writeln!(f, "  this config    {}", self.paths.config_dir.display())?;
        writeln!(f, "  daemon cache   {}", self.status.cache_dir.display())?;
        writeln!(f, "  this cache     {}", self.paths.cache_dir.display())?;
        writeln!(f)?;
        if self.version_skew() {
            writeln!(
                f,
                "This looks like an upgrade boundary. Stop the running daemon, then rerun `{}`:",
                self.verb
            )?;
        } else if self.executable_matches() == Some(false) {
            writeln!(
                f,
                "This looks like a different omnifs build or worktree. Stop it before rerunning `{}`:",
                self.verb
            )?;
        } else {
            writeln!(
                f,
                "Stop or restart the running daemon before rerunning `{}`:",
                self.verb
            )?;
        }
        write!(f, "  omnifs down\n  {}", self.verb)
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    canonical_path(left) == canonical_path(right)
}

fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn display_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        "<unknown>".to_string()
    } else {
        path.display().to_string()
    }
}

/// Print any mounts that failed to converge during reconcile as warnings; a
/// failed mount does not abort the launch, since the rest are serving.
fn report_reconcile_failures(report: &omnifs_api::ReconcileReport) {
    for failure in &report.failed {
        anstream::eprintln!(
            "warning: mount `{}` did not load: {}",
            failure.mount,
            failure.reason
        );
    }
}

/// Docker path: wait for the in-container daemon to serve (it reconciles from
/// `mounts/` on start), then converge once more over the control API to surface
/// any per-mount failure, and write the runtime record host-side (the container
/// cannot reliably own a Unix socket on a macOS bind mount). No spec crosses the
/// wire.
async fn finish_docker_launch(
    rt: &Runtime,
    paths: &WorkspaceLayout,
    target: &DockerTarget,
    token: &str,
) -> anyhow::Result<LaunchOutcome> {
    let addr = published_control_addr();
    let client = DaemonClient::for_tcp(&addr, token.to_string());
    rt.wait_for_daemon_ready(&client).await?;
    client.require_compatible().await?;
    let report = client.reconcile().await?;
    report_reconcile_failures(&report);
    if let Ok(status) = client.status().await {
        report_launch_status(&status);
        write_docker_runtime_record(&paths.runtime_record_file(), &status, target, &addr, token);
    }
    Ok(LaunchOutcome::Docker {
        target: target.clone(),
    })
}

/// Build and persist the host-side runtime record for the Docker bridge.
/// Best-effort: a failure here is logged but does not abort the launch, since
/// the daemon is already serving.
fn write_docker_runtime_record(
    path: &Path,
    status: &DaemonStatus,
    target: &DockerTarget,
    addr: &str,
    token: &str,
) {
    let frontends = status
        .frontend
        .as_ref()
        .map(|frontend| {
            vec![FrontendRecord {
                kind: match frontend.fs_type {
                    FsType::Fuse => RecordFrontendKind::Fuse,
                    FsType::Nfs => RecordFrontendKind::Nfs,
                },
                mount_point: status.mount_point.clone(),
                via: None,
            }]
        })
        .unwrap_or_default();
    let record = RuntimeRecord::new(
        Endpoint::Tcp {
            addr: addr.to_string(),
            token: token.to_string(),
        },
        RecordedBackend::Docker {
            container_name: target.container_name().as_str().to_string(),
            image: target.image().as_str().to_string(),
        },
        status.instance_id.clone(),
        frontends,
    );
    if let Err(error) = record.write(path) {
        anstream::eprintln!("warning: could not write runtime record: {error:#}");
    }
}

fn report_launch_status(status: &DaemonStatus) {
    if let Some(frontend) = status.health.subsystem(DaemonSubsystem::Frontend) {
        anstream::eprintln!("✓ {}", frontend.message);
    } else {
        anstream::eprintln!("✓ Mount is serving at {}", status.mount_point.display());
    }

    if let Some(mounts) = status.health.subsystem(DaemonSubsystem::Mounts) {
        anstream::eprintln!("✓ {}", mounts.message);
    } else {
        anstream::eprintln!("✓ Runtime sees {} provider(s)", status.mounts.len());
    }
}
