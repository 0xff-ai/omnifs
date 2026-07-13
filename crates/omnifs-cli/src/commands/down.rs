//! `omnifs down`: daemon lifecycle stop.
//!
//! Resolution order:
//!   1. Probe the control endpoint: if a live daemon answers, trust
//!      `DaemonStatus.backend` to identify the backend.
//!   2. Fall back to the runtime record: if the daemon is dead, the record
//!      says what was started.
//!   3. If neither applies, nothing is running.
//!
//! The backend is never inferred from `[system].runtime`. `down` is
//! backend-transparent: it dispatches through `LaunchBackend::reclaim` without
//! naming Docker or native.

use clap::Args;

use crate::commands::receipt::TeardownReceipt;
use crate::daemon_teardown::DaemonTeardown;
use crate::error::ExitCode;
use crate::ui::output::{Output, ResultVerdict};
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct DownArgs {
    /// Force the host-native unmount if a clean shutdown leaves the mount busy.
    #[arg(long)]
    pub force: bool,
}

impl DownArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        let DownArgs { force } = self;
        let workspace = Workspace::resolve()?;

        let teardown = DaemonTeardown::new(&workspace, output);
        let exit = if output.is_structured() {
            // The receipt is the whole story: a failed row already conveys the
            // failure, so this returns a non-zero exit code rather than an
            // error that would print a second JSON document.
            let outcomes = teardown.down_collect(force).await?;
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
            teardown.down(force).await?;
            ExitCode::Success
        };
        crate::telemetry::maybe_print_health_nudge(&workspace, output).await;
        Ok(exit)
    }
}
