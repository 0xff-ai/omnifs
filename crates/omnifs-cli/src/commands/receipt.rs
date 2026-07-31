//! Typed structured receipts for the mutating and lifecycle commands.
//!
//! A receipt is the single terminal document a structured command emits on stdout
//! (Part 5 of the agent contract): typed structs, never hand-rolled `json!`,
//! with no human sentences inside values and a machine-visible `fix` on every
//! failed row. All narration stays on stderr. The commands own the side
//! effects; this module owns the wire shape they settle into.

use serde::Serialize;

use crate::ui::consent::{Outcome, OutcomeState};
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
    pub(crate) stopped: usize,
    pub(crate) still_running: Vec<String>,
}

impl TeardownReceipt {
    pub(crate) fn new(rows: Vec<Outcome>, stopped: usize, still_running: Vec<String>) -> Self {
        Self {
            verdict: verdict_from_rows(&rows),
            rows,
            stopped,
            still_running,
        }
    }

    pub(crate) fn exit_code(&self) -> crate::error::ExitCode {
        match self.verdict {
            ResultVerdict::Ok => crate::error::ExitCode::Success,
            ResultVerdict::Degraded => crate::error::ExitCode::GenericFailure,
        }
    }
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
        assert_eq!(json["stopped"], 2);
        assert_eq!(json["still_running"][0], "fuse/host at /mnt/omnifs");
    }
}
