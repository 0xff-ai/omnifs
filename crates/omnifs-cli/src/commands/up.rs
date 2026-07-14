//! `omnifs up`: daemon lifecycle start.

use clap::Args;

use crate::commands::frontend::FrontendController;
use crate::commands::receipt::UpReceipt;
use crate::error::ExitCode;
use crate::inventory::Inventory;
use crate::launch::Launcher;
use crate::ui::output::{Output, ResultVerdict};
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Skip launching any configured frontends after the daemon comes up, on
    /// every OS. The daemon still starts; a frontend already running from a
    /// previous session is untouched and stays usable. Frontends can always
    /// be started later with `omnifs frontend enable`.
    #[arg(long)]
    pub no_frontend: bool,
    /// Wait until /v1/ready answers, failing with exit code 3 on timeout.
    #[arg(long, value_name = "DURATION")]
    pub wait: Option<String>,
}

impl UpArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let wait = self
            .wait
            .as_deref()
            .map(crate::stages::parse_wait_duration)
            .transpose()?;
        let outcome = Launcher::new(&workspace, "omnifs up", output)
            .launch()
            .await?;
        if !output.is_structured() {
            output.narrate("");
        }
        if !output.is_structured() {
            for mount_point in &outcome.local_mount_points {
                output.narrate(format!(
                    "Browse it directly: `{}`",
                    crate::ui::style::bold(format!("ls {}", mount_point.display())),
                ));
            }
        }

        let frontend_degraded = if self.no_frontend {
            false
        } else {
            let report = FrontendController::new(&workspace, output)?
                .converge(outcome.daemon_restarted())
                .await?;
            for failure in &report {
                if !output.is_structured() {
                    output.narrate(format!(
                        "Frontend {} failed: {}\nFix  {}",
                        failure.id,
                        failure.detail.as_deref().unwrap_or("unknown error"),
                        failure.fix.as_deref().unwrap_or("omnifs frontend restart"),
                    ));
                }
            }
            !report.is_empty()
        };

        if let Some(timeout) = wait {
            crate::stages::wait_until_ready(&workspace, timeout).await?;
            if !output.is_structured() {
                output.narrate("Daemon is ready.");
            }
        }
        crate::telemetry::maybe_print_health_nudge(&workspace, output).await;

        if output.is_structured() {
            let code = emit_receipt(&workspace, output).await?;
            return Ok(if frontend_degraded {
                ExitCode::Degraded
            } else {
                code
            });
        }
        Ok(if frontend_degraded {
            ExitCode::Degraded
        } else {
            ExitCode::Success
        })
    }
}

/// Collect the post-launch status and emit the `up` receipt. The verdict and
/// exit code come from the same inventory degraded check that
/// `omnifs status` uses, so a degraded daemon exits 5 here too.
async fn emit_receipt(workspace: &Workspace, output: Output) -> anyhow::Result<ExitCode> {
    let inventory = Inventory::collect(workspace).await?;
    let degraded = inventory.verdict() == crate::inventory::Verdict::Degraded;
    let verdict = if degraded {
        ResultVerdict::Degraded
    } else {
        ResultVerdict::Ok
    };
    output.emit_result(verdict, UpReceipt::from_inventory(inventory))?;
    Ok(if degraded {
        ExitCode::Degraded
    } else {
        ExitCode::Success
    })
}
