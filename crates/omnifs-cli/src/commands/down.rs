//! `omnifs down`: typed daemon shutdown.
//!
//! The command first probes the daemon's typed local control endpoint. If the
//! daemon cannot answer, `DaemonTeardown` falls back to the strict daemon
//! record and its direct pid for liveness-checked cleanup. Frontend runners
//! remain independent throughout either shutdown path.

use clap::Args;

use crate::commands::receipt::TeardownReceipt;
use crate::daemon_teardown::DaemonTeardown;
use crate::error::ExitCode;
use crate::inventory::Inventory;
use crate::ui::output::{Output, ResultVerdict};
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct DownArgs {}

impl DownArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let inventory = Inventory::collect(&workspace).await?;

        let teardown = DaemonTeardown::with_inventory(&workspace, inventory);
        let exit = if output.is_structured() {
            // The receipt is the whole story: a failed row already conveys the
            // failure, so this returns a non-zero exit code rather than an
            // error that would print a second JSON document.
            let outcomes = teardown.down_collect().await?;
            let rows = outcomes
                .iter()
                .map(crate::daemon_teardown::TeardownOutcome::outcome)
                .collect();
            let receipt = TeardownReceipt::new(rows);
            let failed = outcomes
                .iter()
                .any(crate::daemon_teardown::TeardownOutcome::is_failure);
            output.emit_result(
                if failed {
                    ResultVerdict::Degraded
                } else {
                    ResultVerdict::Ok
                },
                receipt,
            )?;
            if failed {
                ExitCode::GenericFailure
            } else {
                ExitCode::Success
            }
        } else {
            teardown.down(&output).await?;
            ExitCode::Success
        };
        crate::metrics::maybe_print_health_nudge(&workspace, output).await;
        Ok(exit)
    }
}
