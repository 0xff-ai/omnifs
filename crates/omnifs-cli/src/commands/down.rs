//! `omnifs down`: typed daemon shutdown.
//!
//! The command first probes the daemon's typed local control endpoint. If the
//! daemon cannot answer, `DaemonTeardown` falls back to the strict daemon
//! record and its direct pid for liveness-checked cleanup. A responsive
//! daemon pushes Stop to Filesystems and reports the bounded drain.

use crate::commands::receipt::TeardownReceipt;
use crate::daemon_teardown::DaemonTeardown;
use crate::error::ExitCode;
use crate::inventory::Inventory;
use crate::ui::output::Output;
use omnifs_bootstrap::Profile;

pub async fn run(output: Output) -> anyhow::Result<ExitCode> {
    let endpoint = Profile::resolve()?;
    let inventory = Inventory::collect_rpc().await?;

    let teardown = DaemonTeardown::with_inventory(endpoint, inventory);
    let exit = if output.is_structured() {
        // The receipt is the whole story: a failed row already conveys the
        // failure, so this returns a non-zero exit code rather than an
        // error that would print a second JSON document.
        let outcomes = teardown.down_collect().await?;
        let (stopped, still_running) = outcomes
            .iter()
            .find_map(|outcome| match outcome {
                crate::daemon_teardown::TeardownOutcome::DaemonStopped {
                    stopped,
                    still_running,
                    ..
                } => Some((*stopped, still_running.clone())),
                _ => None,
            })
            .unwrap_or_default();
        let rows = outcomes
            .iter()
            .map(crate::daemon_teardown::TeardownOutcome::outcome)
            .collect();
        let receipt = TeardownReceipt::new(rows, stopped, still_running);
        let exit = receipt.exit_code();
        output.emit_result(receipt.verdict, receipt)?;
        exit
    } else {
        teardown.down(&output).await?;
        ExitCode::Success
    };
    Ok(exit)
}
