#![allow(clippy::disallowed_macros)] // migrates in wave 5 (cli-redesign)
//! Shared launch choreography for `omnifs up`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use omnifs_api::{API_MAJOR, API_MINOR, DaemonStatus, DaemonSubsystem};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::mounts::materialize;
use omnifs_workspace::provider::Catalog;

use crate::client::{DaemonClient, env_daemon_addr};
use crate::mount_config::MountConfig;
use crate::workspace::Workspace;

/// Command-owned daemon launcher.
///
/// `Launcher` is the policy boundary for `omnifs up`: mount discovery,
/// provider bundle installation, contract preflight, credential preflight,
/// daemon launch, and user-facing next steps stay behind this interface
/// instead of being reassembled by the command.
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
        let telemetry_enabled = config.telemetry_enabled();

        let configs = self.workspace.mounts()?;
        if configs.is_empty() {
            anyhow::bail!(
                "no mount configs found in {}; run `omnifs setup` for guided onboarding, or `omnifs mount add <provider>` to add one directly",
                paths.mounts_dir.display()
            );
        }

        crate::provider_bundle::ensure_providers_installed(&paths.providers_dir)?;
        crate::upgrade::run_upgrade_check(&paths.providers_dir, &configs)?;

        // Fail fast, before the daemon spawns, when a configured mount's
        // host-managed credential is missing or its spec under-grants the
        // pinned provider's declared needs. A daemon spawned anyway would
        // only surface this later as a silent reconcile warning.
        let store = FileStore::new(&paths.credentials_file);
        preflight_mounts(&configs, self.workspace.catalog(), &store)?;

        anstream::eprintln!("Using mount configs from {}", paths.mounts_dir.display());
        launch_host_native(paths, self.verb, telemetry_enabled).await
    }
}

/// Validate every configured mount before the daemon spawns: materialize its
/// spec (capability satisfaction, dynamic-grant resolution) and confirm its
/// host-managed credential, if any, is present. Mirrors the checks the
/// daemon's own reconcile performs per mount, but aborts the whole launch on
/// the first failure instead of recording it and continuing.
fn preflight_mounts(
    configs: &[MountConfig],
    catalog: &Catalog,
    store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    for config in configs {
        let materialized = materialize::materialize(config.config.clone(), catalog)
            .with_context(|| format!("materialize mount {}", config.source.display()))?;
        let mount_auth = crate::auth::MountAuth::from_spec(catalog, materialized);
        config.validate_host_managed_credentials(&mount_auth, store)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct LaunchOutcome {
    pub mount_point: Option<PathBuf>,
}

/// The loopback address the debug `OMNIFS_DAEMON_ADDR` TCP path serves on,
/// honoring the env override.
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
    Ok(LaunchOutcome {
        mount_point: status
            .map(|status| status.mount_point)
            .filter(|mount_point| !mount_point.as_os_str().is_empty()),
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

fn report_launch_status(status: &DaemonStatus) {
    if let Some(frontend) = status.health.subsystem(DaemonSubsystem::Frontend) {
        anstream::eprintln!("✓ {}", frontend.message);
    } else {
        anstream::eprintln!("✓ Namespace daemon is serving");
    }

    if let Some(mounts) = status.health.subsystem(DaemonSubsystem::Mounts) {
        anstream::eprintln!("✓ {}", mounts.message);
    } else {
        anstream::eprintln!("✓ Runtime sees {} provider(s)", status.mounts.len());
    }
}
