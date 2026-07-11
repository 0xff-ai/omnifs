//! Daemon and frontend shutdown workflows.
//!
//! Teardown is deliberately a typed collection step. Commands render these
//! outcomes through the UI event stream, so a reset receipt cannot claim a
//! frontend or daemon was stopped when the cleanup only produced a warning.

use crate::ui::consent::Outcome;
use crate::ui::event::{LedgerRenderer, Render, UiEvent};
use crate::workspace::Workspace;
use omnifs_workspace::runtime_record::{RecordedBackend, RuntimeRecord};

/// One observable teardown result. The variants retain enough context for a
/// command to choose severity and wording without parsing internal prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TeardownOutcome {
    FrontendsRemoved,
    FrontendsAbsent,
    FrontendsFailed { error: String },
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
            Self::FrontendsRemoved | Self::FrontendsAbsent | Self::FrontendsFailed { .. } => {
                "frontends"
            },
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
            Self::FrontendsRemoved => Outcome::done(self.id(), "torn down"),
            Self::FrontendsAbsent => Outcome::skip(self.id(), "none found"),
            Self::FrontendsFailed { error } => {
                Outcome::fail(self.id(), format!("teardown failed: {error}"))
            },
            Self::DaemonStopped { pid } => Outcome::done(self.id(), format!("stopped (pid {pid})")),
            Self::DaemonAlreadyStopped => Outcome::skip(self.id(), "already stopped"),
            Self::DaemonShutdownFailed { error } => {
                Outcome::fail(self.id(), format!("shutdown failed: {error}"))
            },
            Self::StaleRecordRemoved => Outcome::done(self.id(), "stale record removed"),
            Self::StaleRecordAbsent => Outcome::skip(self.id(), "no runtime record"),
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
            Self::FrontendsFailed { .. }
                | Self::DaemonShutdownFailed { .. }
                | Self::StaleRecordKept { .. }
                | Self::OwnershipUnknown { .. }
        )
    }
}

pub(crate) struct DaemonTeardown<'a> {
    workspace: &'a Workspace,
}

impl<'a> DaemonTeardown<'a> {
    pub(crate) fn new(workspace: &'a Workspace) -> Self {
        Self { workspace }
    }

    /// Stop frontends before stopping the namespace daemon they depend on.
    /// Human output is rendered from typed outcomes, not emitted by this
    /// workflow itself.
    pub(crate) async fn down(&self, force: bool) -> anyhow::Result<()> {
        let mut outcomes = Vec::new();
        outcomes.push(self.teardown_frontends(force).await);

        // A live frontend depends on the daemon namespace. Preserve the
        // previous fail-closed behavior: if teardown could not prove that all
        // frontends are gone, do not stop the daemon underneath them.
        if outcomes[0].is_failure() {
            render_outcomes(&outcomes);
            anyhow::bail!(outcomes[0].outcome().value);
        }

        let record_path = self.workspace.layout().runtime_record_file();
        match self.workspace.daemon().status_optional().await {
            Ok(Some(status)) => {
                let outcome = match self.workspace.daemon().shutdown().await {
                    Ok(Some(_)) => TeardownOutcome::DaemonStopped { pid: status.pid },
                    Ok(None) => TeardownOutcome::DaemonAlreadyStopped,
                    Err(error) => TeardownOutcome::DaemonShutdownFailed {
                        error: format!("{error:#}"),
                    },
                };
                if matches!(
                    outcome,
                    TeardownOutcome::DaemonStopped { .. } | TeardownOutcome::DaemonAlreadyStopped
                ) && let Err(error) = RuntimeRecord::remove(&record_path)
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

        render_outcomes(&outcomes);
        if let Some(outcome) = outcomes.iter().find(|outcome| outcome.is_failure()) {
            anyhow::bail!(outcome.outcome().value);
        }
        Ok(())
    }

    /// Best-effort daemon teardown for `omnifs reset`. Every branch is returned
    /// so reset can settle its planned rows truthfully, including warnings.
    pub(crate) async fn reset_best_effort(&self) -> Vec<TeardownOutcome> {
        let mut outcomes = vec![self.teardown_frontends(false).await];
        match self.workspace.daemon().status_optional().await {
            Ok(Some(status)) => {
                outcomes.push(match self.workspace.daemon().shutdown().await {
                    Ok(Some(_)) => TeardownOutcome::DaemonStopped { pid: status.pid },
                    Ok(None) => TeardownOutcome::DaemonAlreadyStopped,
                    Err(error) => TeardownOutcome::DaemonShutdownFailed {
                        error: format!("{error:#}"),
                    },
                });
                if let Some(TeardownOutcome::DaemonShutdownFailed { .. }) = outcomes.last() {
                    // Keep the runtime record so a later `down` can still make
                    // an ownership decision.
                } else if let Err(error) =
                    RuntimeRecord::remove(&self.workspace.layout().runtime_record_file())
                {
                    outcomes.push(TeardownOutcome::StaleRecordKept {
                        error: error.to_string(),
                    });
                }
            },
            Ok(None) => outcomes.push(self.remove_stale_record()),
            Err(error) => outcomes.push(TeardownOutcome::OwnershipUnknown {
                error: format!("{error:#}"),
            }),
        }
        outcomes
    }

    async fn teardown_frontends(&self, force: bool) -> TeardownOutcome {
        let report =
            crate::commands::frontend::down::teardown_report(self.workspace.layout(), force).await;
        if let Some(error) = report.error() {
            TeardownOutcome::FrontendsFailed { error }
        } else if report.found {
            TeardownOutcome::FrontendsRemoved
        } else {
            TeardownOutcome::FrontendsAbsent
        }
    }

    fn remove_stale_record(&self) -> TeardownOutcome {
        let path = self.workspace.layout().runtime_record_file();
        match self.recorded_pid_liveness() {
            Ok(Some(true)) => TeardownOutcome::StaleRecordKept {
                error: "the recorded daemon process is still alive; ownership cannot be verified"
                    .to_owned(),
            },
            Ok(Some(false)) => match RuntimeRecord::remove(&path) {
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
        let Some(record) = RuntimeRecord::read(&self.workspace.layout().runtime_record_file())?
        else {
            return Ok(None);
        };
        let RecordedBackend::Native { pid } = record.backend;
        Ok(Some(crate::process::is_alive(pid)))
    }
}

fn render_outcomes(outcomes: &[TeardownOutcome]) {
    let mut renderer = LedgerRenderer;
    if outcomes.iter().all(|outcome| {
        matches!(
            outcome,
            TeardownOutcome::FrontendsAbsent | TeardownOutcome::StaleRecordAbsent
        )
    }) {
        renderer.event(&UiEvent::Narration {
            message: "Nothing to tear down.".to_owned(),
        });
    }
    for outcome in outcomes {
        let row = outcome.outcome().render_receipt();
        renderer.event(&UiEvent::RowSettled {
            glyph: row.glyph,
            key: row.key,
            value: row.value,
            fix: row.fix,
            duration: None,
        });
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

        let failed = TeardownOutcome::FrontendsFailed {
            error: "busy".to_owned(),
        }
        .outcome();
        assert_eq!(failed.id, "frontends");
        // Frontend teardown failure is fail-closed: it blocks daemon shutdown
        // (see `down`), so its severity is a hard failure, not a warning.
        assert_eq!(failed.glyph(), Glyph::Fail);
        assert!(failed.value.contains("busy"));
    }
}
