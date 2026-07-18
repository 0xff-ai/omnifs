//! Daemon shutdown workflow.
//!
//! Teardown is deliberately a typed collection step. Output renders these
//! outcomes directly, so a receipt cannot claim the daemon was stopped when
//! the cleanup only produced a warning.

use crate::inventory::{DaemonProbe, Inventory};
use crate::ui::consent::Outcome;
use crate::ui::render::{self, Capabilities};
use crate::ui::style;
use omnifs_workspace::Workspace;
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

pub(crate) struct DaemonTeardown {
    client: crate::client::DaemonClient,
    initial: Option<Inventory>,
}

impl DaemonTeardown {
    pub(crate) fn new(workspace: &Workspace) -> Self {
        Self {
            client: crate::client::DaemonClient::for_workspace(workspace),
            initial: None,
        }
    }

    pub(crate) fn with_inventory(workspace: &Workspace, inventory: Inventory) -> Self {
        Self {
            client: crate::client::DaemonClient::for_workspace(workspace),
            initial: Some(inventory),
        }
    }

    /// Stop the namespace daemon and render the typed outcomes through Output.
    /// Bails on the first failure so the exit code reflects an
    /// incomplete teardown.
    pub(crate) async fn down(&self) -> anyhow::Result<()> {
        let outcomes = self.down_collect().await?;
        // `down` is only ever called for human output (`commands/down.rs`
        // routes structured invocations through the receipt path instead),
        // so the whole transcript prints unconditionally: it is this
        // command's entire receipt, not narration a script would want `-q`
        // to drop.
        for line in transcript(&outcomes, crate::ui::output::stderr_capabilities(false)) {
            crate::ui::eprint_raw(&format!("{line}\n"));
        }
        if let Some(outcome) = outcomes.iter().find(|outcome| outcome.is_failure()) {
            anyhow::bail!(outcome.outcome().value);
        }
        Ok(())
    }

    /// Stop only the namespace daemon, leaving every frontend process in
    /// place. Apply uses this path when switching desired mount revisions;
    /// surviving frontends reconnect when the daemon returns.
    pub(crate) async fn stop_daemon(&self) -> anyhow::Result<()> {
        let outcome = match self.client.status_optional().await {
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
                self.client.remove_record()?;
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
        match self.initial_or_status().await {
            Ok(Some(status)) => {
                let outcome = self.shutdown_and_wait(status.pid).await;
                if matches!(
                    outcome,
                    TeardownOutcome::DaemonStopped { .. } | TeardownOutcome::DaemonAlreadyStopped
                ) && let Err(error) = self.client.remove_record()
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

    /// Request shutdown and wait until the control surface and process are gone.
    /// The daemon acknowledges shutdown before its serving task exits, so a
    /// successful POST alone is not enough to report `DaemonStopped`.
    async fn shutdown_and_wait(&self, pid: u32) -> TeardownOutcome {
        match self.client.shutdown().await {
            Ok(Some(())) => {
                let deadline = tokio::time::Instant::now() + SHUTDOWN_SETTLE_TIMEOUT;
                let mut last_error = None;
                loop {
                    match self.client.status_optional().await {
                        Ok(None) if !crate::process::is_alive(pid) => {
                            return TeardownOutcome::DaemonStopped { pid };
                        },
                        Ok(Some(_) | None) => {},
                        Err(error) => last_error = Some(format!("{error:#}")),
                    }
                    if tokio::time::Instant::now() >= deadline {
                        let detail = last_error.map_or_else(
                            || {
                                if crate::process::is_alive(pid) {
                                    "the control surface or daemon process remained alive"
                                        .to_owned()
                                } else {
                                    "the control surface remained reachable".to_owned()
                                }
                            },
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
            _ => self.client.status_optional().await,
        }
    }
    fn remove_stale_record(&self) -> TeardownOutcome {
        match self.recorded_pid_liveness() {
            Ok(Some(true)) => TeardownOutcome::StaleRecordKept {
                error: "the recorded daemon process is still alive; ownership cannot be verified"
                    .to_owned(),
            },
            Ok(Some(false)) => match self.client.remove_record() {
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
        let Some(record) = self.client.record()? else {
            return Ok(None);
        };
        let pid = record.pid;
        Ok(Some(crate::process::is_alive(pid)))
    }
}

/// The exact human lines `down` prints (spec 3.7), pure and independent of
/// the real terminal so it is deterministically testable. Human output shows
/// exactly the `daemon` row: the `runtime-record` bookkeeping outcome is
/// real for the JSON receipt (it can independently fail and needs its own
/// machine-visible row there) but is implementation detail a human never
/// asked to see, so it never reaches this transcript.
///
/// No `daemon`-identified outcome at all means the daemon was never running:
/// whatever else happened was runtime-record bookkeeping, not a stop, so it
/// stays off the human transcript too (an already-absent record needs no
/// cleanup line, and a removed stale record isn't a "stop" either, since
/// nothing was actually running).
/// `down`'s block ever prints exactly one key, `daemon` (spec 3.7): the width
/// is trivially its own display width, computed the same way any other
/// block's shared width is (spec 2.1), even though a single-key block never
/// actually needs alignment against a sibling row.
fn down_key_width() -> usize {
    render::key_field_width(&["daemon"])
}

fn transcript(outcomes: &[TeardownOutcome], caps: Capabilities) -> Vec<String> {
    if outcomes.iter().all(|outcome| outcome.id() != "daemon") {
        return vec!["Nothing to stop. The daemon isn't running.".to_owned()];
    }
    let key_width = down_key_width();
    let mut lines = outcomes
        .iter()
        .filter(|outcome| outcome.id() == "daemon")
        .map(|outcome| render::ledger_row_line(&outcome.outcome().ledger_row(), key_width, caps))
        .collect::<Vec<_>>();
    // Frontends are independent processes and outlive a daemon stop; this is
    // the one fact worth restating, never shown for a failed/no-op teardown.
    if outcomes
        .iter()
        .any(|outcome| matches!(outcome, TeardownOutcome::DaemonStopped { .. }))
    {
        lines.push(style::accentuate(
            "  Frontends stay attached. Your files return with `omnifs up`.",
            caps.color,
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::style::Glyph;

    fn caps(color: bool) -> Capabilities {
        Capabilities {
            width: 120,
            is_tty: color,
            color,
            quiet: false,
        }
    }

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

    /// Spec 3.7, the "daemon was running" branch. `down`'s block has exactly
    /// one key (`daemon`, 6 columns), so the field width is `6 + 3 = 9`, a
    /// 3-space gap rather than the retired fixed-14-column rail's 8:
    /// ```text
    /// ✓ daemon   stopped (pid 31114)
    ///   Frontends stay attached. Your files return with omnifs up.
    /// ```
    #[test]
    fn transcript_matches_the_stopped_daemon_shape() {
        let outcomes = vec![
            TeardownOutcome::DaemonStopped { pid: 31114 },
            TeardownOutcome::StaleRecordRemoved,
        ];
        let lines = transcript(&outcomes, caps(false));
        assert_eq!(
            lines,
            vec![
                "✓ daemon   stopped (pid 31114)".to_owned(),
                "  Frontends stay attached. Your files return with omnifs up.".to_owned(),
            ]
        );
    }

    /// Spec 3.7, the "nothing running" branch: `Nothing to stop. The daemon
    /// isn't running.` No orphan `runtime-record` ledger fragment leaks
    /// through even when a stale record needed cleanup.
    #[test]
    fn transcript_matches_the_nothing_running_shape() {
        assert_eq!(
            transcript(&[TeardownOutcome::StaleRecordAbsent], caps(false)),
            vec!["Nothing to stop. The daemon isn't running.".to_owned()]
        );
        assert_eq!(
            transcript(&[TeardownOutcome::StaleRecordRemoved], caps(false)),
            vec!["Nothing to stop. The daemon isn't running.".to_owned()]
        );
    }

    #[test]
    fn a_teardown_failure_never_shows_the_frontends_stay_attached_line() {
        let outcomes = vec![TeardownOutcome::DaemonShutdownFailed {
            error: "busy".to_owned(),
        }];
        let lines = transcript(&outcomes, caps(false));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("shutdown failed"), "{lines:?}");
    }
}
