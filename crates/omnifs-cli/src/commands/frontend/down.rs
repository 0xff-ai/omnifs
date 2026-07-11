//! `omnifs frontend down`: tear down every frontend owned by this workspace.
//!
//! Attach listeners have no close route on the daemon side
//! (`POST /v1/frontend/attach-target`/`/v1/frontend/attach-target/vsock` only
//! ever bind, idempotently): the listener stays bound until the daemon itself
//! restarts. This command says so rather than implying it closed something it
//! did not.
//!
//! [`teardown`] is shared with `omnifs down`, which tears down frontends before
//! stopping the daemon.

use clap::Args;
use omnifs_workspace::layout::WorkspaceLayout;

use crate::frontend_backend::{DockerBackend, FrontendBackend};
use crate::frontend_container::{FRONTEND_DEV_IMAGE, frontend_container_name};
use crate::krunkit_backend::KrunkitBackend;
use crate::launch_backend::DockerTarget;
use crate::runtime::Runtime;
use crate::ui::event::{LedgerRenderer, Render, UiEvent};
use crate::ui::style::Glyph;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct FrontendDownArgs {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TeardownRow {
    pub(crate) glyph: Glyph,
    pub(crate) key: String,
    pub(crate) value: String,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct TeardownReport {
    pub(crate) found: bool,
    pub(crate) rows: Vec<TeardownRow>,
    pub(crate) failures: Vec<String>,
}

impl TeardownReport {
    pub(crate) fn error(&self) -> Option<String> {
        (!self.failures.is_empty())
            .then(|| format!("frontend teardown incomplete: {}", self.failures.join("; ")))
    }
}

impl FrontendDownArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let found = teardown(workspace.layout(), false).await?;
        if found {
            let mut renderer = LedgerRenderer;
            renderer.event(&UiEvent::Narration {
                message:
                    "The daemon's namespace attach listener stays bound until the daemon restarts."
                        .to_owned(),
            });
        }
        Ok(())
    }
}

/// Remove every frontend discoverable for this workspace.
pub(crate) async fn teardown(paths: &WorkspaceLayout, force: bool) -> anyhow::Result<bool> {
    let report = teardown_report(paths, force).await;
    render_report(&report);
    if let Some(error) = report.error() {
        anyhow::bail!(error);
    }
    Ok(report.found)
}

/// Collect frontend teardown outcomes without rendering. Daemon shutdown and
/// reset consume this report so one lifecycle operation has one output owner.
pub(crate) async fn teardown_report(paths: &WorkspaceLayout, force: bool) -> TeardownReport {
    #[cfg(not(feature = "daemon"))]
    let _ = force;
    let mut report = TeardownReport::default();
    let krunkit = KrunkitBackend::new(paths.config_dir.clone());
    match krunkit.is_running().await {
        Ok(Some(_)) => match krunkit.tear_down().await {
            Ok(()) => {
                report.found = true;
                report.rows.push(TeardownRow {
                    glyph: Glyph::Done,
                    key: "frontend krunkit".to_owned(),
                    value: "removed".to_owned(),
                });
            },
            Err(error) => {
                report
                    .failures
                    .push(format!("remove krunkit frontend: {error:#}"));
            },
        },
        Ok(None) => {},
        Err(error) => {
            report
                .failures
                .push(format!("inspect krunkit frontend: {error:#}"));
        },
    }

    match teardown_docker(paths).await {
        Ok(true) => {
            report.found = true;
            report.rows.push(TeardownRow {
                glyph: Glyph::Done,
                key: "frontend docker".to_owned(),
                value: "removed".to_owned(),
            });
        },
        Ok(false) => {},
        Err(error) => {
            report
                .failures
                .push(format!("inspect or remove Docker frontend: {error:#}"));
        },
    }

    #[cfg(feature = "daemon")]
    match crate::host_teardown::teardown_local_frontends(&paths.frontend_state_root(), force) {
        Ok(summary) => {
            let local_found = summary.unmounted > 0 || summary.swept_orphans > 0;
            if local_found {
                report.found = true;
                report.rows.push(TeardownRow {
                    glyph: Glyph::Done,
                    key: "frontend local".to_owned(),
                    value: format!(
                        "removed ({} unmounted, {} orphan records swept)",
                        summary.unmounted, summary.swept_orphans
                    ),
                });
            }
            if !summary.failed.is_empty() {
                report.failures.push(format!(
                    "{} local frontend(s) could not be safely unmounted",
                    summary.failed.len()
                ));
            }
            if summary.skipped > 0 {
                report.failures.push(format!(
                    "{} local frontend record(s) could not be read",
                    summary.skipped
                ));
            }
            report.failures.extend(summary.errors);
        },
        Err(error) => report
            .failures
            .push(format!("inspect local frontend state: {error:#}")),
    }

    report
}

fn render_report(report: &TeardownReport) {
    let mut renderer = LedgerRenderer;
    if report.rows.is_empty() && report.failures.is_empty() {
        renderer.event(&UiEvent::Narration {
            message: "No frontend found.".to_owned(),
        });
        return;
    }
    for row in &report.rows {
        renderer.event(&UiEvent::RowSettled {
            glyph: row.glyph,
            key: row.key.clone(),
            value: row.value.clone(),
            fix: None,
            duration: None,
        });
    }
    for failure in &report.failures {
        renderer.event(&UiEvent::RowSettled {
            glyph: Glyph::Fail,
            key: "frontend".to_owned(),
            value: failure.clone(),
            fix: None,
            duration: None,
        });
    }
}

async fn teardown_docker(paths: &WorkspaceLayout) -> anyhow::Result<bool> {
    let container_name = frontend_container_name(paths)?;

    // The image field is unused by removal; it only needs to be a valid
    // reference, so the dev placeholder is fine regardless of build channel.
    let target = DockerTarget::new(
        container_name.as_str().to_string(),
        FRONTEND_DEV_IMAGE.to_string(),
    )?;
    let runtime = Runtime::connect_for(&target).map_err(|error| {
        anyhow::anyhow!(
            "Docker not reachable; could not check for frontend container `{container_name}`: {error}"
        )
    })?;
    let Some(discovered) = runtime
        .frontend_container_for_home(&paths.config_dir)
        .await?
    else {
        return Ok(false);
    };
    let discovered_target = DockerTarget::new(
        discovered.as_str().to_string(),
        FRONTEND_DEV_IMAGE.to_string(),
    )?;
    let backend = DockerBackend::new(Runtime::connect_for(&discovered_target)?);
    let running = backend.is_running().await?.is_some();
    if running {
        backend.tear_down().await?;
    }
    Ok(running)
}
