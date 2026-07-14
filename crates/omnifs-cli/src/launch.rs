//! Shared launch choreography for `omnifs up`.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use omnifs_api::{DaemonStatus, DaemonSubsystem};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::mounts::{Registry, Revision};
use omnifs_workspace::provider::Catalog;
use omnifs_workspace::runtime_record::RuntimeRecord;

use crate::client::DaemonClient;
use crate::daemon_teardown::DaemonTeardown;
use crate::mount_config::MountConfig;
use crate::ui::output::Output;
use crate::workspace::Workspace;

/// Command-owned daemon launcher.
///
/// `Launcher` is the policy boundary for `omnifs up`: mount discovery,
/// exact-pinned mount discovery, contract preflight, credential preflight,
/// daemon launch, and user-facing next steps stay behind this interface
/// instead of being reassembled by the command.
pub(crate) struct Launcher<'a> {
    workspace: &'a Workspace,
    verb: &'static str,
    output: Output,
}

impl<'a> Launcher<'a> {
    pub(crate) fn new(workspace: &'a Workspace, verb: &'static str, output: Output) -> Self {
        Self {
            workspace,
            verb,
            output,
        }
    }

    pub(crate) async fn launch(self) -> anyhow::Result<()> {
        let paths = self.workspace.layout();
        let config = self.workspace.config()?;
        let telemetry_enabled =
            config.telemetry.enabled && omnifs_workspace::telemetry::enabled_from_env();

        let revision = self.workspace.commit_mounts()?;
        let (snapshot_dir, snapshot) = self
            .workspace
            .repository()?
            .snapshot(&revision, &paths.cache_dir)?;
        let configs = mount_configs(&snapshot);
        if configs.is_empty() {
            anyhow::bail!(
                "no mount configs found in {}; run `omnifs mount add <provider>` to create one",
                paths.mounts_dir.display()
            );
        }

        // Fail fast, before a healthy daemon is stopped or a new daemon spawns,
        // when a configured mount's host-managed credential is missing. The
        // daemon resolves the pinned manifest and bound config into authority
        // before constructing any provider instance.
        let store = FileStore::new(&paths.credentials_file);
        preflight_mounts(&configs, self.workspace.catalog(), &store)?;

        self.output.narrate(format!(
            "Applying mount revision {} from {}",
            revision,
            snapshot_dir.display()
        ));
        launch_host_native(
            self.workspace,
            self.verb,
            telemetry_enabled,
            self.output,
            &revision,
            &snapshot_dir,
        )
        .await?;
        self.workspace.repository()?.mark_applied(&revision)?;
        Ok(())
    }
}

fn mount_configs(registry: &Registry) -> Vec<MountConfig> {
    registry
        .iter()
        .map(|(name, spec)| MountConfig {
            name: name.clone(),
            config: spec.clone(),
            source: registry.spec_path(name),
        })
        .collect()
}

/// Validate every configured mount before the running daemon is touched and
/// confirm its host-managed credential, if any, is present. Authority belongs
/// to the new daemon startup, so this preflight stays credential-only and a
/// failed authority resolution leaves a healthy prior revision serving.
fn preflight_mounts(
    configs: &[MountConfig],
    catalog: &Catalog,
    store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    for config in configs {
        let mount_auth = crate::auth::MountAuth::from_spec(catalog, config.config.clone());
        config.validate_host_managed_credentials(&mount_auth, store)?;
    }
    Ok(())
}

/// Leave a daemon already serving `revision` alone, or replace only the daemon
/// process and wait for the immutable snapshot to become ready.
async fn launch_host_native(
    workspace: &Workspace,
    verb: &str,
    telemetry_enabled: bool,
    output: Output,
    revision: &Revision,
    snapshot: &Path,
) -> anyhow::Result<()> {
    let paths = workspace.layout();
    let client = DaemonClient::for_layout(paths);
    let current = client.status_optional().await?;

    if let Some(status) = &current {
        let existing = ExistingDaemon::new(status.clone(), paths, verb);
        if !existing.can_apply() {
            anyhow::bail!(existing);
        }
        let serves_revision =
            RuntimeRecord::read(&paths.runtime_record_file())?.is_some_and(|record| {
                record.instance_id == status.instance_id && record.mount_revision == *revision
            });
        if serves_revision {
            report_launch_status(status);
            return Ok(());
        }

        output.narrate("Restarting omnifs daemon for changed mount revision");
        DaemonTeardown::new(workspace).stop_daemon().await?;
    } else {
        output.narrate("Starting omnifs daemon (host-native)");
    }

    crate::launch_backend::launch_native(paths, telemetry_enabled, revision, snapshot).await?;

    let status = client.status().await?;
    report_launch_status(&status);
    Ok(())
}

#[derive(Debug)]
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

    /// True when the running daemon's build version differs from this CLI's.
    fn version_skew(&self) -> bool {
        self.status.version != env!("CARGO_PKG_VERSION")
    }

    fn can_apply(&self) -> bool {
        !self.version_skew() && self.paths_match() && self.executable_matches() != Some(false)
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
            "  daemon  v{}  pid {}  {}",
            self.status.version,
            self.status.pid,
            display_path(self.daemon_executable())
        )?;
        writeln!(
            f,
            "  this    v{}       {}",
            env!("CARGO_PKG_VERSION"),
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

fn report_launch_status(status: &DaemonStatus) {
    if let Some(frontend) = status.health.subsystem(DaemonSubsystem::Frontend) {
        crate::ui::eprint_raw(&format!("✓ {}\n", frontend.message));
    } else {
        crate::ui::eprint_raw("✓ Namespace daemon is serving\n");
    }

    if let Some(mounts) = status.health.subsystem(DaemonSubsystem::Mounts) {
        crate::ui::eprint_raw(&format!("✓ {}\n", mounts.message));
    } else {
        crate::ui::eprint_raw(&format!(
            "✓ Runtime serves {} mount(s)\n",
            status.mounts.len()
        ));
    }
}
