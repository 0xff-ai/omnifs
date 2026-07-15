//! Daemon shutdown workflow.
//!
//! Teardown is deliberately a typed collection step. Output renders these
//! outcomes directly, so a receipt cannot claim the daemon was stopped when
//! the cleanup only produced a warning.

use crate::inventory::{DaemonProbe, Inventory};
use crate::ui::consent::Outcome;
use crate::ui::output::Output;
use crate::workspace::Workspace;
use omnifs_workspace::daemon_record::DaemonRecord;
use std::time::Duration;

const SHUTDOWN_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One observable teardown result. The variants retain enough context for a
/// command to choose severity and wording without parsing internal prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TeardownOutcome {
    DaemonStopped { pid: u32 },
    DaemonAlreadyStopped,
    DaemonShutdownFailed { error: String },
    StaleRecordRemoved,
    StaleRecordAbsent,
    StaleRecordKept { error: String },
    OwnershipUnknown { error: String },
}

impl TeardownOutcome {
    pub(crate) fn id(&self) -> &'static str {
        match self {
            Self::DaemonStopped { .. }
            | Self::DaemonAlreadyStopped
            | Self::DaemonShutdownFailed { .. }
            | Self::OwnershipUnknown { .. } => "daemon",
            Self::StaleRecordRemoved | Self::StaleRecordAbsent | Self::StaleRecordKept { .. } => {
                "runtime-record"
            },
        }
    }

    pub(crate) fn outcome(&self) -> Outcome {
        match self {
            Self::DaemonStopped { pid } => Outcome::done(self.id(), format!("stopped (pid {pid})")),
            Self::DaemonAlreadyStopped => Outcome::skip(self.id(), "already stopped"),
            Self::DaemonShutdownFailed { error } => {
                Outcome::fail(self.id(), format!("shutdown failed: {error}"))
            },
            Self::StaleRecordRemoved => Outcome::done(self.id(), "stale record removed"),
            Self::StaleRecordAbsent => Outcome::skip(self.id(), "no daemon record"),
            Self::StaleRecordKept { error } => {
                Outcome::fail(self.id(), format!("record kept: {error}"))
            },
            Self::OwnershipUnknown { error } => Outcome::fail(
                self.id(),
                format!("ownership could not be verified: {error}"),
            ),
        }
    }

    pub(crate) fn is_failure(&self) -> bool {
        matches!(
            self,
            Self::DaemonShutdownFailed { .. }
                | Self::StaleRecordKept { .. }
                | Self::OwnershipUnknown { .. }
        )
    }
}

pub(crate) struct DaemonTeardown<'a> {
    workspace: &'a Workspace,
    initial: Option<Inventory>,
}

impl<'a> DaemonTeardown<'a> {
    pub(crate) fn new(workspace: &'a Workspace) -> Self {
        Self {
            workspace,
            initial: None,
        }
    }

    pub(crate) fn with_inventory(workspace: &'a Workspace, inventory: Inventory) -> Self {
        Self {
            workspace,
            initial: Some(inventory),
        }
    }

    /// Stop the namespace daemon and render the typed outcomes through Output.
    /// Bails on the first failure so the exit code reflects an
    /// incomplete teardown.
    pub(crate) async fn down(&self, output: &Output) -> anyhow::Result<()> {
        let outcomes = self.down_collect().await?;
        render_outcomes(output, &outcomes);
        if let Some(outcome) = outcomes.iter().find(|outcome| outcome.is_failure()) {
            anyhow::bail!(outcome.outcome().value);
        }
        Ok(())
    }

    /// Stop only the namespace daemon, leaving every frontend process in
    /// place. Apply uses this path when switching desired mount revisions;
    /// surviving frontends reconnect when the daemon returns.
    pub(crate) async fn stop_daemon(&self) -> anyhow::Result<()> {
        let record_path = self.workspace.layout().daemon_record_file();
        let outcome = match self.workspace.daemon().status_optional().await {
            Ok(Some(status)) => self.shutdown_and_wait(status.pid).await,
            Ok(None) => self.remove_stale_record(),
            Err(error) => anyhow::bail!(
                "cannot stop the running daemon before applying desired state: {error:#}"
            ),
        };

        match outcome {
            TeardownOutcome::DaemonStopped { .. }
            | TeardownOutcome::DaemonAlreadyStopped
            | TeardownOutcome::StaleRecordRemoved
            | TeardownOutcome::StaleRecordAbsent => {
                DaemonRecord::remove(&record_path)?;
                Ok(())
            },
            failure => anyhow::bail!(failure.outcome().value),
        }
    }

    /// Run the daemon teardown workflow and return its typed outcomes without
    /// rendering. `down` renders these through Output; structured output settles
    /// them into a receipt.
    pub(crate) async fn down_collect(&self) -> anyhow::Result<Vec<TeardownOutcome>> {
        let mut outcomes = Vec::new();
        let record_path = self.workspace.layout().daemon_record_file();
        match self.initial_or_status().await {
            Ok(Some(status)) => {
                let outcome = self.shutdown_and_wait(status.pid).await;
                if matches!(
                    outcome,
                    TeardownOutcome::DaemonStopped { .. } | TeardownOutcome::DaemonAlreadyStopped
                ) && let Err(error) = DaemonRecord::remove(&record_path)
                {
                    outcomes.push(TeardownOutcome::StaleRecordKept {
                        error: error.to_string(),
                    });
                }
                outcomes.push(outcome);
            },
            Ok(None) => outcomes.push(self.remove_stale_record()),
            Err(error) => match self.recorded_pid_liveness()? {
                Some(true) => {
                    let outcome = TeardownOutcome::OwnershipUnknown {
                        error: format!(
                            "daemon status failed while the recorded process is still alive; \
                             stop it manually, then retry: {error:#}"
                        ),
                    };
                    outcomes.push(outcome);
                },
                Some(false) => outcomes.push(self.remove_stale_record()),
                None => return Err(error),
            },
        }
        Ok(outcomes)
    }

    /// Request shutdown and wait until the control surface is observably gone.
    /// The daemon acknowledges shutdown before its serving task exits, so a
    /// successful POST alone is not enough to report `DaemonStopped`.
    async fn shutdown_and_wait(&self, pid: u32) -> TeardownOutcome {
        match self.workspace.daemon().shutdown().await {
            Ok(Some(_)) => {
                let deadline = tokio::time::Instant::now() + SHUTDOWN_SETTLE_TIMEOUT;
                let mut last_error = None;
                loop {
                    match self.workspace.daemon().status_optional().await {
                        Ok(None) => return TeardownOutcome::DaemonStopped { pid },
                        Ok(Some(_)) => {},
                        Err(error) => last_error = Some(format!("{error:#}")),
                    }
                    if tokio::time::Instant::now() >= deadline {
                        let detail = last_error.map_or_else(
                            || "the control surface remained reachable".to_owned(),
                            |error| format!("the control surface could not be verified: {error}"),
                        );
                        return TeardownOutcome::DaemonShutdownFailed {
                            error: format!(
                                "shutdown acknowledged but daemon did not become unavailable within {}s; {detail}",
                                SHUTDOWN_SETTLE_TIMEOUT.as_secs()
                            ),
                        };
                    }
                    tokio::time::sleep(SHUTDOWN_POLL_INTERVAL).await;
                }
            },
            Ok(None) => TeardownOutcome::DaemonAlreadyStopped,
            Err(error) => TeardownOutcome::DaemonShutdownFailed {
                error: format!("{error:#}"),
            },
        }
    }

    async fn initial_or_status(&self) -> anyhow::Result<Option<omnifs_api::DaemonStatus>> {
        match self.initial.as_ref().map(|inventory| &inventory.daemon) {
            Some(daemon) if daemon.probe == DaemonProbe::Stopped => Ok(None),
            Some(daemon) if daemon.probe == DaemonProbe::Responding => Ok(daemon.status.clone()),
            _ => self.workspace.daemon().status_optional().await,
        }
    }
    fn remove_stale_record(&self) -> TeardownOutcome {
        let path = self.workspace.layout().daemon_record_file();
        match self.recorded_pid_liveness() {
            Ok(Some(true)) => TeardownOutcome::StaleRecordKept {
                error: "the recorded daemon process is still alive; ownership cannot be verified"
                    .to_owned(),
            },
            Ok(Some(false)) => match DaemonRecord::remove(&path) {
                Ok(()) => TeardownOutcome::StaleRecordRemoved,
                Err(error) => TeardownOutcome::StaleRecordKept {
                    error: error.to_string(),
                },
            },
            Ok(None) => TeardownOutcome::StaleRecordAbsent,
            Err(error) => TeardownOutcome::StaleRecordKept {
                error: error.to_string(),
            },
        }
    }

    fn recorded_pid_liveness(&self) -> anyhow::Result<Option<bool>> {
        let Some(record) = DaemonRecord::read(&self.workspace.layout().daemon_record_file())?
        else {
            return Ok(None);
        };
        let pid = record.pid;
        Ok(Some(crate::process::is_alive(pid)))
    }
}

fn render_outcomes(output: &Output, outcomes: &[TeardownOutcome]) {
    if outcomes
        .iter()
        .all(|outcome| matches!(outcome, TeardownOutcome::StaleRecordAbsent))
    {
        output.narrate("Nothing to tear down.");
    }
    for outcome in outcomes {
        let row = outcome.outcome().render_receipt();
        output.row(&row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::style::Glyph;

    #[test]
    fn teardown_outcomes_have_truthful_severity_and_ids() {
        let stopped = TeardownOutcome::DaemonStopped { pid: 42 }.outcome();
        assert_eq!(stopped.id, "daemon");
        assert_eq!(stopped.glyph(), Glyph::Done);

        let failed = TeardownOutcome::DaemonShutdownFailed {
            error: "busy".to_owned(),
        }
        .outcome();
        assert_eq!(failed.id, "daemon");
        // Daemon shutdown failure is a hard failure, not a warning.
        assert_eq!(failed.glyph(), Glyph::Fail);
        assert!(failed.value.contains("busy"));
    }
}
