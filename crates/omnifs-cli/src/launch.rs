//! Shared launch choreography for `omnifs up`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use omnifs_api::{DaemonStatus, DaemonSubsystem};
use omnifs_workspace::creds::CredentialStore;
use omnifs_workspace::mounts::{Registry, Revision};
use omnifs_workspace::provider::Catalog;

use crate::client::DaemonClient;
use crate::daemon_teardown::DaemonTeardown;
use crate::mount_config::MountConfig;
use crate::ui::output::Output;
use crate::ui::report::Row;
use crate::ui::style::Glyph;
use omnifs_workspace::Workspace;

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
    offline: bool,
    readiness_timeout: Duration,
}

impl<'a> Launcher<'a> {
    pub(crate) fn new(
        workspace: &'a Workspace,
        verb: &'static str,
        output: Output,
        offline: bool,
        readiness_timeout: Duration,
    ) -> Self {
        Self {
            workspace,
            verb,
            output,
            offline,
            readiness_timeout,
        }
    }

    pub(crate) async fn launch(self) -> anyhow::Result<()> {
        let desired_state = self.workspace.desired_state();
        let config = self.workspace.config()?;
        let metrics_enabled =
            config.metrics.enabled && omnifs_workspace::metrics::enabled_from_env();

        let (revision, snapshot_dir, snapshot) = if self.offline {
            anyhow::ensure!(
                desired_state.repository_exists(),
                "offline startup requires an existing mount repository at {}",
                desired_state.repository_display()
            );
            let repository = desired_state.observe_repository()?;
            let revision = repository
                .head_revision()?
                .ok_or_else(|| anyhow::anyhow!("offline startup requires a current mount HEAD"))?;
            let (snapshot_dir, snapshot) = desired_state.snapshot(&repository, &revision)?;
            (revision, snapshot_dir, snapshot)
        } else {
            let revision = desired_state.commit()?;
            let repository = desired_state.repository()?;
            let (snapshot_dir, snapshot) = desired_state.snapshot(&repository, &revision)?;
            (revision, snapshot_dir, snapshot)
        };
        let configs = mount_configs(&snapshot);
        if configs.is_empty() {
            anyhow::bail!(
                "no mount configs found in {}; run `omnifs mount add <provider>` to create one",
                desired_state.repository_display()
            );
        }

        if self.offline {
            self.output.narrate(format!(
                "Starting offline daemon from mount revision {revision}"
            ));
            self.launch_host_native(metrics_enabled, &revision, &snapshot_dir, true)
                .await?;
            return Ok(());
        }

        // Fail fast, before a healthy daemon is stopped or a new daemon spawns,
        // when a configured mount's host-managed credential is missing. The
        // daemon resolves the pinned manifest and bound config into authority
        // before constructing any provider instance.
        preflight_mounts(
            &configs,
            self.workspace.catalog(),
            self.workspace.credentials(),
        )?;

        let warmup = crate::provider_warmup::ProviderWarmup::new(
            self.workspace.warmup().clone(),
            self.workspace.catalog().clone(),
        )
        .warm_for_up(
            configs.iter().map(|config| config.config.provider.id),
            &self.output,
        )
        .await?;

        self.output.narrate(format!(
            "Applying mount revision {} from {}",
            revision,
            snapshot_dir.display()
        ));
        self.launch_host_native(metrics_enabled, &revision, &snapshot_dir, false)
            .await?;
        drop(warmup);
        desired_state.repository()?.mark_applied(&revision)?;
        Ok(())
    }

    /// Leave a daemon already serving `revision` alone, or replace only the
    /// daemon process and wait for the immutable snapshot to become ready.
    async fn launch_host_native(
        &self,
        metrics_enabled: bool,
        revision: &Revision,
        snapshot: &Path,
        offline: bool,
    ) -> anyhow::Result<()> {
        let client = crate::client::DaemonClient::for_workspace(self.workspace);
        let current = client.status_optional().await?;

        if let Some(status) = &current {
            let existing = ExistingDaemon::new(status.clone(), &client, self.verb);
            if !existing.can_apply() {
                anyhow::bail!(existing);
            }
            let existing_record = client.record()?;
            let serves_revision = existing_record.as_ref().is_some_and(|record| {
                record.instance_id == status.instance_id
                    && record.mount_revision == *revision
                    && record.offline == offline
            });
            if serves_revision {
                report_launch_status(&self.output, status);
                return Ok(());
            }

            if existing_record.as_ref().is_some_and(|record| {
                record.mount_revision == *revision && record.offline != offline
            }) {
                self.output
                    .narrate("Restarting omnifs daemon for changed online/offline mode");
            } else {
                self.output
                    .narrate("Restarting omnifs daemon for changed mount revision");
            }
            if offline {
                client
                    .validate_offline(revision)
                    .await
                    .context("validate offline projection before replacing daemon")?;
            }
            DaemonTeardown::new(self.workspace).stop_daemon().await?;
        } else {
            self.output.narrate("Starting omnifs daemon (host-native)");
        }

        crate::daemon_launch::launch(
            &client,
            metrics_enabled,
            revision,
            snapshot,
            offline,
            self.readiness_timeout,
        )
        .await?;

        let status = client.status().await?;
        report_launch_status(&self.output, &status);
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

#[derive(Debug)]
struct ExistingDaemon {
    status: DaemonStatus,
    paths_match: bool,
    config_display: String,
    cache_display: String,
    verb: String,
}

impl ExistingDaemon {
    fn new(status: DaemonStatus, client: &DaemonClient, verb: &str) -> Self {
        let paths_match = client.matches_status(&status);
        Self {
            status,
            paths_match,
            config_display: client.config_display(),
            cache_display: client.cache_display(),
            verb: verb.to_string(),
        }
    }

    fn daemon_executable(&self) -> &Path {
        self.status.executable.as_path()
    }

    fn paths_match(&self) -> bool {
        self.paths_match
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
        writeln!(f, "  this config    {}", self.config_display)?;
        writeln!(f, "  daemon cache   {}", self.status.cache_dir.display())?;
        writeln!(f, "  this cache     {}", self.cache_display)?;
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

fn report_launch_status(output: &Output, status: &DaemonStatus) {
    if let Some(frontend) = status.health.subsystem(DaemonSubsystem::Frontend) {
        output.row(&Row::new(Glyph::Done, "frontend", frontend.message.clone()));
    } else {
        output.row(&Row::new(
            Glyph::Done,
            "frontend",
            "namespace daemon is serving",
        ));
    }

    if let Some(mounts) = status.health.subsystem(DaemonSubsystem::Mounts) {
        output.row(&Row::new(Glyph::Done, "mounts", mounts.message.clone()));
    } else {
        output.row(&Row::new(
            Glyph::Done,
            "mounts",
            format!("runtime serves {} mount(s)", status.mounts.len()),
        ));
    }
}
