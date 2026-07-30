//! Typed structured receipts for the mutating and lifecycle commands.
//!
//! A receipt is the single terminal document a structured command emits on stdout
//! (Part 5 of the agent contract): typed structs, never hand-rolled `json!`,
//! with no human sentences inside values and a machine-visible `fix` on every
//! failed row. All narration stays on stderr. The commands own the side
//! effects; this module owns the wire shape they settle into.

use serde::Serialize;

use crate::commands::mount::MountInitStatus;
use crate::ui::consent::{Outcome, OutcomeState, Plan};
use crate::ui::output::ResultVerdict;

/// Derive a receipt verdict from its settled rows: `degraded` if any row
/// failed, `ok` otherwise. The one place that DERIVES a verdict from
/// outcomes; every other receipt in this module hardcodes a literal because
/// reaching its construction point already proves what happened (see each
/// constructor's own comment for why).
fn verdict_from_rows(rows: &[Outcome]) -> ResultVerdict {
    if rows.iter().any(|row| row.state == OutcomeState::Fail) {
        ResultVerdict::Degraded
    } else {
        ResultVerdict::Ok
    }
}

/// `omnifs down`: the settled operation rows and a verdict. `Degraded` marks
/// a receipt whose exit code is non-zero even
/// though the document itself is the whole story (no separate error document).
#[derive(Debug, Serialize)]
pub(crate) struct TeardownReceipt {
    pub(crate) verdict: ResultVerdict,
    pub(crate) rows: Vec<Outcome>,
    pub(crate) detached: usize,
    pub(crate) still_attached: Vec<String>,
}

/// `omnifs mount rm`: the approved removal plan and the rows settled by the
/// operation. Dry runs retain the plan while leaving `rows` empty because no
/// operation was applied.
#[derive(Debug, Serialize)]
pub(crate) struct MountRemoveReceipt {
    pub(crate) verdict: ResultVerdict,
    pub(crate) mount: String,
    pub(crate) rows: Vec<Outcome>,
    pub(crate) dry_run: bool,
    pub(crate) plan: Plan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) revision: Option<u64>,
}

impl MountRemoveReceipt {
    pub(crate) fn dry_run(mount: String, plan: Plan) -> Self {
        Self {
            verdict: ResultVerdict::Ok,
            mount,
            rows: Vec::new(),
            dry_run: true,
            plan,
            revision: None,
        }
    }

    pub(crate) fn applied(
        mount: String,
        plan: Plan,
        rows: Vec<Outcome>,
        revision: Option<u64>,
    ) -> Self {
        Self {
            verdict: verdict_from_rows(&rows),
            mount,
            rows,
            dry_run: false,
            plan,
            revision,
        }
    }
}

impl TeardownReceipt {
    pub(crate) fn new(rows: Vec<Outcome>, detached: usize, still_attached: Vec<String>) -> Self {
        Self {
            verdict: verdict_from_rows(&rows),
            rows,
            detached,
            still_attached,
        }
    }

    pub(crate) fn exit_code(&self) -> crate::error::ExitCode {
        match self.verdict {
            ResultVerdict::Ok => crate::error::ExitCode::Success,
            ResultVerdict::Degraded => crate::error::ExitCode::GenericFailure,
        }
    }
}

/// `omnifs mount add`: the mount that was written and whether its
/// credential is live yet.
#[derive(Debug, Serialize)]
pub(crate) struct MountAddReceipt {
    pub(crate) verdict: ResultVerdict,
    pub(crate) mount: String,
    pub(crate) status: MountInitStatus,
    pub(crate) revision: String,
}

/// `omnifs mount reauth`: the mount whose credential was refreshed. Reaching
/// this constructor already proves success: any failure to reauthenticate
/// propagates as an error before a receipt is ever built, so there is no
/// outcome here for a verdict to be derived from.
#[derive(Debug, Serialize)]
pub(crate) struct MountReauthReceipt {
    pub(crate) verdict: ResultVerdict,
    pub(crate) mount: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_receipt_owns_its_terminal_result() {
        let receipt = TeardownReceipt::new(
            vec![Outcome::fail("daemon", "still running")],
            2,
            vec!["fuse/host at /mnt/omnifs".to_owned()],
        );

        assert_eq!(receipt.verdict, ResultVerdict::Degraded);
        assert_eq!(receipt.exit_code(), crate::error::ExitCode::GenericFailure);
        let json = serde_json::to_value(&receipt).unwrap();
        assert_eq!(json["verdict"], "degraded");
        assert_eq!(json["detached"], 2);
        assert_eq!(json["still_attached"][0], "fuse/host at /mnt/omnifs");
    }
}
