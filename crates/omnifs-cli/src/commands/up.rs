//! `omnifs up`: daemon lifecycle start.

use clap::Args;

use crate::commands::receipt::UpReceipt;
use crate::error::ExitCode;
use crate::inventory::Inventory;
use crate::launch::Launcher;
use crate::ui::output::{Output, ResultVerdict};
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Wait until /v1/ready answers, failing with exit code 3 on timeout.
    #[arg(long, value_name = "DURATION")]
    pub wait: Option<String>,
}

impl UpArgs {
    pub(crate) async fn start_in_workspace(
        &self,
        workspace: &Workspace,
        output: Output,
    ) -> anyhow::Result<()> {
        let wait = self
            .wait
            .as_deref()
            .map(crate::stages::parse_wait_duration)
            .transpose()?;
        Launcher::new(workspace, "omnifs up", output)
            .launch()
            .await?;

        if let Some(timeout) = wait {
            crate::stages::wait_until_ready(workspace, timeout).await?;
            if !output.is_structured() {
                output.narrate("Daemon is ready.");
            }
        }
        Ok(())
    }

    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        self.start_in_workspace(&workspace, output).await?;
        crate::telemetry::maybe_print_health_nudge(&workspace, output).await;

        if output.is_structured() {
            return emit_receipt(&workspace, output).await;
        }
        Ok(ExitCode::Success)
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
