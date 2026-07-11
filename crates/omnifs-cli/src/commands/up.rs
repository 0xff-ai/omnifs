#![allow(clippy::disallowed_macros)] // migrates in wave 5 (cli-redesign)
//! `omnifs up`: daemon lifecycle start.

use anyhow::Context as _;
use clap::Args;

use crate::commands::frontend::up::{first_mount_name, launch_entry};
use crate::commands::receipt::UpReceipt;
use crate::config::{EffectiveFrontend, HostOs, Provenance, resolve_frontends};
use crate::error::ExitCode;
use crate::frontend_backend::Driver;
use crate::launch::Launcher;
use crate::status::StatusReport;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Skip launching any configured frontends after the daemon comes up, on
    /// every OS. The daemon still starts; a frontend already running from a
    /// previous session is untouched and stays usable. Frontends can always
    /// be started later with `omnifs frontend up`.
    #[arg(long)]
    pub no_frontend: bool,
    /// Wait until /v1/ready answers, failing with exit code 3 on timeout.
    #[arg(long, value_name = "DURATION")]
    pub wait: Option<String>,
    /// Emit a machine-readable JSON receipt (daemon, mounts, frontends, and a
    /// verdict) on stdout. Exits 5 when the verdict is degraded.
    #[arg(long)]
    pub json: bool,
}

impl UpArgs {
    pub async fn run(self) -> anyhow::Result<ExitCode> {
        if self.json {
            crate::ui::output::note_json_receipt();
        }
        let workspace = Workspace::resolve()?;
        let wait = self
            .wait
            .as_deref()
            .map(crate::stages::parse_wait_duration)
            .transpose()?;
        let outcome = Launcher::new(&workspace, "omnifs up").launch().await?;
        crate::ui::narrate("");
        if let Some(mount_point) = &outcome.mount_point {
            crate::ui::narrate(format!(
                "Browse it directly: `{}`",
                crate::style::bold(format!("ls {}", mount_point.display())),
            ));
        }

        if !self.no_frontend {
            launch_frontends(&workspace).await?;
        }

        if let Some(timeout) = wait {
            crate::stages::wait_until_ready(&workspace, timeout).await?;
            crate::ui::narrate("Daemon is ready.");
        }
        crate::telemetry::maybe_print_health_nudge(&workspace).await;

        if self.json {
            return emit_receipt(&workspace).await;
        }
        Ok(ExitCode::Success)
    }
}

/// Collect the post-launch status and emit the `up` receipt. The verdict and
/// exit code come from the same `StatusReport` degraded check that
/// `omnifs status` uses, so a degraded daemon exits 5 here too.
async fn emit_receipt(workspace: &Workspace) -> anyhow::Result<ExitCode> {
    let mounts = workspace.mounts()?;
    let runtime = workspace.daemon().compatible_status_optional().await?;
    let report = StatusReport::collect(
        workspace.catalog(),
        workspace.layout().clone(),
        runtime,
        &mounts,
    );
    let degraded = report.exit_code() == ExitCode::Degraded;
    crate::ui::print_json(&UpReceipt::from_status(report.into_json(), degraded))?;
    Ok(if degraded {
        ExitCode::Degraded
    } else {
        ExitCode::Success
    })
}

/// Launch every frontend in the effective `[[frontends]]` plan (explicit
/// config, else the platform default). Local entries launch first so the
/// soft-fail rule below is meaningful: only the implicit macOS Docker
/// default (a default-provenance entry using the docker driver) may fail
/// with a warning, once the local entry it follows has already succeeded.
/// Every other entry failure — any explicit entry, or a local default — is
/// fatal to `omnifs up`.
async fn launch_frontends(workspace: &Workspace) -> anyhow::Result<()> {
    let config = workspace.config()?;
    let default_mount_point = omnifs_workspace::layout::resolve_mount_point()
        .context("cannot resolve the default mount point: set HOME or OMNIFS_MOUNT_POINT")?;
    let plan = resolve_frontends(&config.frontends, HostOs::current(), &default_mount_point)?;
    let mount_name = first_mount_name(workspace)?;

    let (locals, guests): (Vec<_>, Vec<_>) = plan
        .into_iter()
        .partition(|entry| entry.driver == Driver::Local);

    for entry in &locals {
        launch_entry(workspace, entry, &mount_name)
            .await
            .with_context(|| format!("start the local {} frontend", entry.kind.label()))?;
    }

    for entry in &guests {
        announce(entry);
        let result = launch_entry(workspace, entry, &mount_name).await;
        let soft_fail = entry.provenance == Provenance::Default && entry.driver == Driver::Docker;
        match (result, soft_fail) {
            (Ok(()), _) => {},
            (Err(error), true) => {
                let driver_label = entry.driver.as_via().label();
                crate::ui::narrate(format!(
                    "⚠  Could not start the {driver_label} frontend: {error:#}"
                ));
                crate::ui::narrate(crate::ui::note(
                    "the mount above is still available; run `omnifs frontend up` to retry, or pass --no-frontend to skip it",
                ));
            },
            (Err(error), false) => {
                return Err(error).with_context(|| {
                    format!("start the {} frontend", entry.driver.as_via().label())
                });
            },
        }
    }

    Ok(())
}

fn announce(entry: &EffectiveFrontend) {
    crate::ui::narrate("");
    crate::ui::narrate(format!(
        "Starting the {} {} frontend...",
        entry.driver.as_via().label(),
        entry.kind.label()
    ));
}
