//! Shared launch choreography for `omnifs up` and `omnifs dev`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use omnifs_api::{API_MAJOR, API_MINOR, DaemonStatus, DaemonSubsystem};
use omnifs_creds::{CredentialStore, FileStore};
use omnifs_home::WorkspaceLayout;
use omnifs_mount::materialize::{self, MaterializationMode, MaterializedMount};

use crate::backend::LaunchParams;
use crate::catalog::ProviderCatalog;
use crate::client::DaemonClient;
use crate::launch_backend::{DockerTarget, LaunchBackend};
use crate::launch_record::LaunchRecord;
use crate::runtime::{ContainerExtras, Runtime};
use crate::session::MountConfig;
use crate::workspace::Workspace;

/// Command-owned daemon launcher.
///
/// `Launcher` is the policy boundary for `omnifs up`: setup state, backend
/// resolution, mount discovery, provider bundle installation, contract
/// preflight, runtime launch, and user-facing next steps stay behind this
/// interface instead of being reassembled by the command.
pub(crate) struct Launcher<'a> {
    workspace: &'a Workspace,
    verb: &'static str,
}

impl<'a> Launcher<'a> {
    pub(crate) fn new(workspace: &'a Workspace, verb: &'static str) -> Self {
        Self { workspace, verb }
    }

    pub(crate) async fn launch(self) -> anyhow::Result<LaunchOutcome> {
        let paths = self.workspace.layout();
        let config = self.workspace.config()?;
        if config.system.runtime.is_none() {
            anyhow::bail!(
                "`{}` requires setup to choose a daemon backend; run `omnifs setup` first",
                self.verb
            );
        }
        let backend = LaunchBackend::from_config(&config)?;

        let configs = self.workspace.mounts()?;
        if configs.is_empty() {
            anyhow::bail!(
                "no mount configs found in {}; run `omnifs setup` for guided onboarding, or `omnifs init <provider>` to add one directly",
                paths.mounts_dir.display()
            );
        }

        crate::provider_bundle::ensure_providers_installed(&paths.providers_dir)?;

        crate::upgrade::run_upgrade_check(&paths.mounts_dir, &paths.providers_dir, &configs)?;

        anstream::println!("Using mount configs from {}", paths.mounts_dir.display());
        launch_runtime(
            LaunchSpec {
                backend,
                paths,
                store: Box::new(FileStore::new(&paths.credentials_file)),
                verb: self.verb,
                configs,
                extras: ContainerExtras::default(),
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
    pub extras: ContainerExtras,
}

/// Docker-specific mount materialization for launch-time container binds.
///
/// The daemon still reads specs from `mounts/` and reconciles them itself. This
/// materializer exists only for the container invariant Docker imposes: host
/// preopen directories must be known before `docker create`, while credential
/// failures should still surface before the daemon starts.
pub(crate) struct DockerMountMaterializer<'a> {
    catalog: &'a ProviderCatalog,
    store: &'a dyn CredentialStore,
}

impl<'a> DockerMountMaterializer<'a> {
    pub(crate) fn new(catalog: &'a ProviderCatalog, store: &'a dyn CredentialStore) -> Self {
        Self { catalog, store }
    }

    pub(crate) fn materialize(&self, config: &MountConfig) -> anyhow::Result<MaterializedMount> {
        let materialized = materialize::materialize(
            config.config.clone(),
            self.catalog.inner(),
            MaterializationMode::Docker,
        )
        .with_context(|| format!("materialize mount {}", config.source.display()))?;

        let resolved = self
            .catalog
            .resolve_mount_spec(materialized.spec().clone(), false)
            .with_context(|| format!("resolve mount config for {}", config.source.display()))?;
        let mount_auth = self
            .catalog
            .resolve_mount_auth_tolerating_manifest_errors(resolved);
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
    catalog: &ProviderCatalog,
) -> anyhow::Result<LaunchOutcome> {
    let LaunchSpec {
        backend,
        paths,
        store,
        verb,
        configs,
        mut extras,
    } = spec;

    std::fs::create_dir_all(&paths.config_dir)
        .with_context(|| format!("create runtime home {}", paths.config_dir.display()))?;

    let target = match backend {
        LaunchBackend::Native => return launch_host_native(paths, verb).await,
        LaunchBackend::Docker(target) => target,
    };

    anstream::println!("Computing container binds for {} mount(s)", configs.len());
    let preopen_binds =
        DockerMountMaterializer::new(catalog, store.as_ref()).materialize_bind_specs(&configs)?;
    let extra_binds = std::mem::take(&mut extras.binds);
    extras.binds = preopen_binds.into_iter().chain(extra_binds).collect();

    let rt = Runtime::connect_ready(&target, verb).await?;
    rt.launch_container(&paths.config_dir, extras).await?;

    match finish_docker_launch(&rt, paths, &target).await {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            if let Err(teardown) = rt.remove().await {
                anstream::eprintln!("also failed to remove the container: {teardown:#}");
            }
            Err(error)
        },
    }
}

/// Parse a `host:port` string into a `SocketAddr`, falling back to
/// `127.0.0.1:DEFAULT_PORT` on any parse error.
fn parse_control_addr(addr: &str) -> SocketAddr {
    addr.parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], omnifs_api::DEFAULT_PORT)))
}

/// Read the daemon control address from the environment (`OMNIFS_DAEMON_ADDR`),
/// falling back to `127.0.0.1:DEFAULT_PORT` on any parse error. Both the
/// spawned daemon (`--listen`) and the client (`DaemonClient::new`) use this
/// so a per-test override moves them together.
fn resolve_control_addr() -> SocketAddr {
    parse_control_addr(&crate::inspector::daemon_addr())
}

/// Spawn a detached host-native daemon and wait for it to serve. The daemon
/// reconciles `mounts/` on start; the CLI triggers one more reconcile to
/// converge any change since and to surface per-mount failures.
async fn launch_host_native(paths: &WorkspaceLayout, verb: &str) -> anyhow::Result<LaunchOutcome> {
    reject_existing_host_daemon(paths, verb).await?;
    anstream::println!("Starting omnifs daemon (host-native)");

    // Build the params and delegate spawn+wait to the backend abstraction so
    // the native path and Docker path share the same argument generator.
    let addr = resolve_control_addr();
    let params = LaunchParams {
        paths: paths.clone(),
        control_addr: addr,
        mount_point: None, // the daemon resolves its default
        backend: LaunchBackend::Native,
    };
    crate::backend::launch_native(&params).await?;

    let client = DaemonClient::new();
    match client.reconcile().await {
        Ok(report) => report_reconcile_failures(&report),
        Err(error) => {
            return Err(error);
        },
    }

    // Read daemon status to get the mount point and PID for the launch record.
    let status = client.status().await.ok();
    if let Some(status) = &status {
        report_launch_status(status);
        let record_params = LaunchParams {
            paths: paths.clone(),
            control_addr: addr,
            mount_point: Some(status.mount_point.clone()),
            backend: LaunchBackend::Native,
        };
        write_launch_record(&paths.config_dir, &record_params, Some(status.pid));
    }
    Ok(LaunchOutcome::Native {
        mount_point: status.map(|status| status.mount_point),
    })
}

async fn reject_existing_host_daemon(paths: &WorkspaceLayout, verb: &str) -> anyhow::Result<()> {
    let client = DaemonClient::new();
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

    fn title(&self) -> &'static str {
        if self.status.api_major != API_MAJOR || self.status.version != env!("CARGO_PKG_VERSION") {
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
        if self.status.api_major != API_MAJOR || self.status.version != env!("CARGO_PKG_VERSION") {
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
/// any per-mount failure. No spec crosses the wire.
async fn finish_docker_launch(
    rt: &Runtime,
    paths: &WorkspaceLayout,
    target: &DockerTarget,
) -> anyhow::Result<LaunchOutcome> {
    rt.wait_for_daemon_ready().await?;
    let client = DaemonClient::new();
    client.require_compatible().await?;
    let report = client.reconcile().await?;
    report_reconcile_failures(&report);
    if let Ok(status) = client.status().await {
        report_launch_status(&status);
        let addr: SocketAddr = format!("127.0.0.1:{}", omnifs_api::DEFAULT_PORT)
            .parse()
            .expect("static address is valid");
        let record_params = LaunchParams {
            paths: paths.clone(),
            control_addr: addr,
            mount_point: Some(status.mount_point.clone()),
            backend: LaunchBackend::Docker(target.clone()),
        };
        write_launch_record(&paths.config_dir, &record_params, None);
    }
    Ok(LaunchOutcome::Docker {
        target: target.clone(),
    })
}

/// Build and persist the launch record at `<config_dir>/launch.json`.
/// Best-effort: a failure here is logged but does not abort the launch, since
/// the daemon is already serving.
fn write_launch_record(config_dir: &Path, params: &LaunchParams, daemon_pid: Option<u32>) {
    match LaunchRecord::new(params, daemon_pid) {
        Ok(record) => {
            if let Err(error) = record.write(config_dir) {
                anstream::eprintln!("warning: could not write launch record: {error:#}");
            }
        },
        Err(error) => {
            anstream::eprintln!("warning: could not build launch record: {error:#}");
        },
    }
}

fn report_launch_status(status: &DaemonStatus) {
    if let Some(frontend) = status.health.subsystem(DaemonSubsystem::Frontend) {
        anstream::println!("✓ {}", frontend.message);
    } else {
        anstream::println!("✓ Mount is serving at {}", status.mount_point.display());
    }

    if let Some(mounts) = status.health.subsystem(DaemonSubsystem::Mounts) {
        anstream::println!("✓ {}", mounts.message);
    } else {
        anstream::println!("✓ Runtime sees {} provider(s)", status.mounts.len());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_control_addr_handles_input() {
        let bad = super::parse_control_addr("not-a-valid-socket-addr!!!");
        assert_eq!(bad.port(), omnifs_api::DEFAULT_PORT);
        assert_eq!(bad.ip(), std::net::IpAddr::from([127, 0, 0, 1]));

        let good = super::parse_control_addr("127.0.0.1:19999");
        assert_eq!(good.port(), 19999);
        assert_eq!(good.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
    }
}
