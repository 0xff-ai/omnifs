//! Shared launch choreography for `omnifs up`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use omnifs_api::DaemonStatus;
use omnifs_workspace::creds::CredentialStore;
use omnifs_workspace::mounts::{Registry, Revision};
use omnifs_workspace::provider::Catalog;

use crate::client::DaemonClient;
use crate::commands::frontend::{FrontendFilesystem as Filesystem, FrontendRuntime as Runtime};
use crate::daemon_teardown::DaemonTeardown;
use crate::inventory::{FrontendState, FrontendStatus, Inventory};
use crate::mount_config::MountConfig;
use crate::ui::live::LiveRegion;
use crate::ui::output::Output;
use crate::ui::render::LedgerRow;
use crate::ui::style::Glyph;
use omnifs_workspace::Workspace;

/// A short grace window for independent frontend runners to reattach after
/// this command replaces the daemon they were talking to.
const RECONNECT_GRACE: Duration = Duration::from_secs(3);
const RECONNECT_POLL: Duration = Duration::from_millis(250);

/// The keys `up`'s ledger block ever prints, in print order. Rows settle one
/// at a time as async work finishes rather than as one batch (provider
/// warmup's spinner settles first, then `daemon`/`mounts` print together,
/// then `frontends` settles last from the reconnect-grace live region), so
/// the shared key width is computed once from this fixed set up front: a
/// block's key column is sized to the whole block, never truncated.
pub(crate) const UP_LEDGER_KEYS: [&str; 4] = ["providers", "daemon", "mounts", "frontends"];

pub(crate) fn up_key_width() -> usize {
    crate::ui::render::key_field_width(&UP_LEDGER_KEYS)
}

/// Whether replacing the daemon actually happened, or the daemon was already
/// serving the desired revision. Only a real replacement prints the ledger
/// block, waits out the frontend reconnect grace, and (for the human
/// register) prints access lines below it; a no-op collapses to the single
/// `Already serving revision <sha>. Files at <location>` sentence instead.
pub(crate) enum LaunchOutcome {
    AlreadyServing,
    Started,
}

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

    pub(crate) async fn launch(self) -> anyhow::Result<LaunchOutcome> {
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

        // A no-op invocation collapses to one sentence and does none of the
        // work below: checked before provider warmup or any
        // narration runs, not just before the ledger rows print, so a
        // `--offline` or already-serving `up` never even joins the warmup
        // lock it has nothing to do with.
        if Self::already_serving(self.workspace, &revision, self.offline).await? {
            return Ok(LaunchOutcome::AlreadyServing);
        }

        if self.offline {
            self.output.narrate(format!(
                "Starting offline daemon from mount revision {revision}"
            ));
            return self
                .launch_host_native(metrics_enabled, &revision, &snapshot_dir, true)
                .await;
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
            up_key_width(),
        )
        .await?;

        self.output
            .narrate(format!("Applying mount revision {revision}"));
        let outcome = self
            .launch_host_native(metrics_enabled, &revision, &snapshot_dir, false)
            .await?;
        drop(warmup);
        desired_state.repository()?.mark_applied(&revision)?;
        Ok(outcome)
    }

    /// True when a reachable daemon already serves exactly this revision and
    /// online/offline mode, so this invocation has nothing to do. A cheap
    /// early peek: [`Launcher::launch_host_native`] re-checks the same
    /// condition right before it would otherwise replace the daemon, so a
    /// race between the two only ever costs a redundant no-op, never a
    /// missed one.
    async fn already_serving(
        workspace: &Workspace,
        revision: &Revision,
        offline: bool,
    ) -> anyhow::Result<bool> {
        let client = crate::client::DaemonClient::for_workspace(workspace);
        let Some(status) = client.status_optional().await? else {
            return Ok(false);
        };
        let Some(record) = client.record()? else {
            return Ok(false);
        };
        Ok(record.instance_id == status.instance_id
            && record.mount_revision == *revision
            && record.offline == offline)
    }

    /// Leave a daemon already serving `revision` alone, or replace only the
    /// daemon process and wait for the immutable snapshot to become ready.
    async fn launch_host_native(
        &self,
        metrics_enabled: bool,
        revision: &Revision,
        snapshot: &Path,
        offline: bool,
    ) -> anyhow::Result<LaunchOutcome> {
        let client = crate::client::DaemonClient::for_workspace(self.workspace);
        let current = client.status_optional().await?;

        // The frontends observed while the prior daemon was still answering
        // are the set expected to reattach once the new one is ready. A
        // fresh start (no prior daemon) has nothing to reconnect: any
        // frontend runner sitting idle without ever having attached is an
        // `enable`, not a `reattach`, and stays out of this slice's grace
        // wait.
        let mut expected_reattach = Vec::new();

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
                return Ok(LaunchOutcome::AlreadyServing);
            }

            if let Ok(inventory) = Inventory::collect(self.workspace).await {
                expected_reattach = inventory
                    .frontends
                    .iter()
                    .filter(|frontend| frontend.state == FrontendState::Attached)
                    .map(FrontendTrack::from_status)
                    .collect();
            }

            if existing_record.as_ref().is_some_and(|record| {
                record.mount_revision == *revision && record.offline != offline
            }) {
                self.output
                    .narrate("Restarting for changed online/offline mode");
            } else {
                self.output.narrate("Restarting for changed mount revision");
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
        let key_width = up_key_width();
        report_launch_status(&self.output, &status, revision, offline, key_width);
        if !expected_reattach.is_empty() {
            self.wait_for_reattachment(expected_reattach, key_width)
                .await?;
        }
        Ok(LaunchOutcome::Started)
    }

    /// Wait a short grace period for every frontend observed just before
    /// replacement to reappear as attached, rendering the live-region row
    /// and settled summary from the live region. Timing out is not a failure:
    /// `up` still returns `Ok`, and the caller's exit code stays 0.
    async fn wait_for_reattachment(
        &self,
        expected: Vec<FrontendTrack>,
        key_width: usize,
    ) -> anyhow::Result<()> {
        let total = expected.len();
        let mut region = LiveRegion::new(self.output.clone(), ["frontends"]);
        let deadline = tokio::time::Instant::now() + RECONNECT_GRACE;
        let client = crate::client::DaemonClient::for_workspace(self.workspace);
        loop {
            // Reattachment only ever shows up as a live daemon attachment, so
            // poll the daemon status directly instead of re-running the full
            // `Inventory::collect` join (registry parse, credential lookups,
            // runner discovery I/O) every tick for one signal.
            let status = client.status_optional().await.ok().flatten();
            let observed = crate::inventory::frontend_statuses(status.as_ref(), 0, Vec::new());
            let observed = observed.as_slice();
            let (reattached, pending) = reattach_progress(&expected, observed);
            region.update("frontends", format!("{reattached}/{total} reattached…"));

            if pending.is_empty() {
                let detail = expected
                    .iter()
                    .map(FrontendTrack::describe)
                    .collect::<Vec<_>>()
                    .join(", ");
                region.finish(
                    Glyph::Done,
                    "frontends",
                    reattached_value(reattached, total, &detail),
                    key_width,
                );
                return Ok(());
            }

            if tokio::time::Instant::now() >= deadline {
                let first = &pending[0];
                region.finish(
                    Glyph::Warn,
                    "frontends",
                    pending_value(reattached, total, &first.describe()),
                    key_width,
                );
                self.output
                    .narrate(format!("  fix:  `{}`", first.restart_command()));
                return Ok(());
            }

            match LiveRegion::race(tokio::time::sleep(RECONNECT_POLL)).await {
                Ok(()) => {},
                Err(canceled) => {
                    let (reattached, _) = reattach_progress(&expected, observed);
                    region.cancel(
                        Glyph::Warn,
                        "frontends",
                        format!("{reattached}/{total} reattached (canceled)"),
                        key_width,
                    );
                    return Err(canceled.into());
                },
            }
        }
    }
}

/// A frontend's stable identity across a daemon replacement: enough to
/// match a fresh [`FrontendStatus`] observation and build its restart
/// command if it never comes back within the grace window.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FrontendTrack {
    filesystem: Filesystem,
    runtime: Runtime,
    location: Option<PathBuf>,
}

impl FrontendTrack {
    fn from_status(status: &FrontendStatus) -> Self {
        Self {
            filesystem: status.filesystem,
            runtime: status.runtime,
            location: status.location.clone(),
        }
    }

    fn matches(&self, status: &FrontendStatus) -> bool {
        self.filesystem == status.filesystem
            && self.runtime == status.runtime
            && self.location == status.location
    }

    fn describe(&self) -> String {
        format!("{} {}", self.filesystem.label(), self.runtime.label())
    }

    fn restart_command(&self) -> String {
        format!(
            "omnifs frontend restart {} --runtime {}",
            self.filesystem.label(),
            self.runtime.label()
        )
    }
}

/// The settled `frontends` row value once every expected track reattached.
fn reattached_value(reattached: usize, total: usize, detail: &str) -> String {
    format!("{reattached}/{total} reattached ({detail})")
}

/// The settled `frontends` row value when the grace window elapsed with at
/// least one track still pending.
fn pending_value(reattached: usize, total: usize, first_pending: &str) -> String {
    format!("{reattached}/{total} reattached, {first_pending} pending")
}

/// How many of `expected` are observed attached in `current`, and which
/// tracks are still pending. Pure so the reconnect-grace state machine is
/// unit-testable without a real daemon or real timing.
fn reattach_progress(
    expected: &[FrontendTrack],
    current: &[FrontendStatus],
) -> (usize, Vec<FrontendTrack>) {
    let pending: Vec<FrontendTrack> = expected
        .iter()
        .filter(|track| {
            !current
                .iter()
                .any(|status| status.state == FrontendState::Attached && track.matches(status))
        })
        .cloned()
        .collect();
    (expected.len() - pending.len(), pending)
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

/// The `daemon` row's value, `running (pid 31114), revision 3f69473`, or
/// `running (offline, revision <sha>)` for `--offline`). Pure so
/// the exact wording is testable without a live daemon.
fn daemon_row_value(pid: u32, revision: &Revision, offline: bool) -> String {
    if offline {
        format!("running (offline, revision {revision})")
    } else {
        format!("running (pid {pid}), revision {revision}")
    }
}

/// The `mounts` row's value. Pure so the
/// exact wording is testable without a live daemon.
fn mounts_row_value(mounts: &[omnifs_api::MountInfo]) -> String {
    let mount_names = mounts
        .iter()
        .map(|mount| format!("/{}", mount.mount.trim_start_matches('/')))
        .collect::<Vec<_>>()
        .join(" ");
    if mount_names.is_empty() {
        "none serving".to_owned()
    } else {
        format!("{mount_names} serving")
    }
}

/// Print the `daemon` and `mounts` rows of `up`'s ledger block.
/// The former generic `frontend`/`mounts` subsystem-health rows are gone:
/// `daemon` now carries the identity a successful launch actually answers
/// (pid, revision), and the frontend row is owned entirely by
/// [`Launcher::wait_for_reattachment`] below.
fn report_launch_status(
    output: &Output,
    status: &DaemonStatus,
    revision: &Revision,
    offline: bool,
    key_width: usize,
) {
    output.ledger_row(
        &LedgerRow::new(
            Glyph::Done,
            "daemon",
            daemon_row_value(status.pid, revision, offline),
        ),
        key_width,
    );
    output.ledger_row(
        &LedgerRow::new(Glyph::Done, "mounts", mounts_row_value(&status.mounts)),
        key_width,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::render::Capabilities;

    fn caps() -> Capabilities {
        Capabilities {
            width: 120,
            is_tty: false,
            color: false,
            quiet: false,
        }
    }

    fn revision() -> Revision {
        Revision::new("3f69473".to_owned() + &"0".repeat(33)).expect("test revision")
    }

    fn mount_info(name: &str) -> omnifs_api::MountInfo {
        omnifs_api::MountInfo {
            mount: name.to_owned(),
            provider_name: name.to_owned(),
            provider_id: "a".repeat(64),
            auth_health: None,
        }
    }

    #[test]
    fn daemon_row_value_matches_the_documented_shape() {
        let revision = revision();
        assert_eq!(
            daemon_row_value(31114, &revision, false),
            format!("running (pid 31114), revision {revision}")
        );
        assert_eq!(
            daemon_row_value(31114, &revision, true),
            format!("running (offline, revision {revision})")
        );
    }

    #[test]
    fn mounts_row_value_lists_every_mount_root_space_joined() {
        let mounts = vec![mount_info("github"), mount_info("dns")];
        assert_eq!(mounts_row_value(&mounts), "/github /dns serving");
        assert_eq!(mounts_row_value(&[]), "none serving");
    }

    #[test]
    fn reattach_summary_values_match_the_documented_shapes() {
        assert_eq!(
            reattached_value(2, 2, "nfs host, fuse libkrun"),
            "2/2 reattached (nfs host, fuse libkrun)"
        );
        assert_eq!(
            pending_value(1, 2, "fuse libkrun"),
            "1/2 reattached, fuse libkrun pending"
        );
    }

    fn track(filesystem: Filesystem, runtime: Runtime, location: &str) -> FrontendTrack {
        FrontendTrack {
            filesystem,
            runtime,
            location: Some(PathBuf::from(location)),
        }
    }

    fn attached(track: &FrontendTrack) -> FrontendStatus {
        FrontendStatus {
            filesystem: track.filesystem,
            runtime: track.runtime,
            location: track.location.clone(),
            state: FrontendState::Attached,
            scope: "all",
            mount_count: 2,
            fix: None,
        }
    }

    #[test]
    fn reattach_progress_counts_matched_tracks_and_lists_the_rest_as_pending() {
        let nfs_host = track(Filesystem::Nfs, Runtime::Host, "/Users/raul/omnifs");
        let fuse_libkrun = track(Filesystem::Fuse, Runtime::Libkrun, "/omnifs");
        let expected = vec![nfs_host.clone(), fuse_libkrun.clone()];

        let (reattached, pending) = reattach_progress(&expected, &[attached(&nfs_host)]);
        assert_eq!(reattached, 1);
        assert_eq!(pending, vec![fuse_libkrun.clone()]);

        let (reattached, pending) =
            reattach_progress(&expected, &[attached(&nfs_host), attached(&fuse_libkrun)]);
        assert_eq!(reattached, 2);
        assert!(pending.is_empty());

        let (reattached, pending) = reattach_progress(&expected, &[]);
        assert_eq!(reattached, 0);
        assert_eq!(pending, expected);
    }

    #[test]
    fn reattach_progress_ignores_a_track_observed_but_not_yet_attached() {
        let track = track(Filesystem::Fuse, Runtime::Docker, "/omnifs");
        let mut not_yet_attached = attached(&track);
        not_yet_attached.state = FrontendState::Running;
        let (reattached, pending) =
            reattach_progress(std::slice::from_ref(&track), &[not_yet_attached]);
        assert_eq!(reattached, 0);
        assert_eq!(pending, vec![track]);
    }

    #[test]
    fn frontend_track_restart_command_names_the_exact_filesystem_and_runtime() {
        let track = track(Filesystem::Fuse, Runtime::Libkrun, "/omnifs");
        assert_eq!(track.describe(), "fuse libkrun");
        assert_eq!(
            track.restart_command(),
            "omnifs frontend restart fuse --runtime libkrun"
        );
    }

    /// The `daemon`/`mounts`/`frontends` three-row fragment of the
    /// ledger block, reproduced byte-for-byte from the same pure value
    /// functions and the same streamed-row primitive `report_launch_status`/
    /// `wait_for_reattachment` call in production:
    /// ```text
    /// ✓ daemon      running (pid 31114), revision 3f69473...
    /// ✓ mounts      /github /dns serving
    /// ✓ frontends   2/2 reattached (nfs host, fuse libkrun)
    /// ```
    #[test]
    fn up_ledger_rows_compose_into_the_documented_block() {
        let revision = revision();
        let width = up_key_width();
        let rows = [
            crate::ui::render::LedgerRow::new(
                Glyph::Done,
                "daemon",
                daemon_row_value(31114, &revision, false),
            ),
            crate::ui::render::LedgerRow::new(
                Glyph::Done,
                "mounts",
                mounts_row_value(&[mount_info("github"), mount_info("dns")]),
            ),
            crate::ui::render::LedgerRow::new(
                Glyph::Done,
                "frontends",
                reattached_value(2, 2, "nfs host, fuse libkrun"),
            ),
        ];
        let block = rows
            .iter()
            .map(|row| crate::ui::render::ledger_row_line(row, width, caps()))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            block,
            format!(
                "✓ daemon      running (pid 31114), revision {revision}\n\
                 ✓ mounts      /github /dns serving\n\
                 ✓ frontends   2/2 reattached (nfs host, fuse libkrun)"
            )
        );
    }
}
