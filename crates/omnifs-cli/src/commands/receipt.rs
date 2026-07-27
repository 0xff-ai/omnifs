//! Typed structured receipts for the mutating and lifecycle commands.
//!
//! A receipt is the single terminal document a structured command emits on stdout
//! (Part 5 of the agent contract): typed structs, never hand-rolled `json!`,
//! with no human sentences inside values and a machine-visible `fix` on every
//! failed row. All narration stays on stderr. The commands own the side
//! effects; this module owns the wire shape they settle into.

use serde::Serialize;

use crate::inventory::Inventory;
use crate::stages::MountInitStatus;
use crate::ui::consent::{Outcome, Plan};
use crate::ui::output::ResultVerdict;

/// The overall health of a completed operation. `up` reports `degraded` (and
/// exits 5) when any mount, filesystem, or subsystem needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    Ok,
    Degraded,
    Failed,
}

impl Verdict {
    fn from_rows(rows: &[Outcome]) -> Self {
        if rows
            .iter()
            .any(|row| row.state == crate::ui::consent::OutcomeState::Fail)
        {
            Self::Failed
        } else {
            Self::Ok
        }
    }

    const fn output_verdict(self) -> ResultVerdict {
        match self {
            Self::Ok => ResultVerdict::Ok,
            Self::Degraded | Self::Failed => ResultVerdict::Degraded,
        }
    }
}

/// `omnifs up`: the daemon, its mounts and filesystems, and a verdict.
/// Reuses the same status view types so the receipt never drifts from
/// `omnifs status`.
#[derive(Debug, Serialize)]
pub(crate) struct UpReceipt {
    pub(crate) verdict: Verdict,
    pub(crate) inventory: Inventory,
}

impl UpReceipt {
    pub(crate) fn from_inventory(inventory: Inventory) -> Self {
        let degraded = inventory.verdict() == crate::inventory::Verdict::Degraded;
        Self {
            verdict: if degraded {
                Verdict::Degraded
            } else {
                Verdict::Ok
            },
            inventory,
        }
    }
}

/// `omnifs down`: the settled operation rows and a verdict. `Failed` marks a
/// receipt whose exit code is non-zero even
/// though the document itself is the whole story (no separate error document).
#[derive(Debug, Serialize)]
pub(crate) struct TeardownReceipt {
    pub(crate) verdict: Verdict,
    pub(crate) rows: Vec<Outcome>,
    pub(crate) detached: usize,
    pub(crate) still_attached: Vec<String>,
}

/// `omnifs mount rm`: the approved removal plan and the rows settled by the
/// operation. Dry runs retain the plan while leaving `rows` empty because no
/// operation was applied.
#[derive(Debug, Serialize)]
pub(crate) struct MountRemoveReceipt {
    pub(crate) verdict: Verdict,
    pub(crate) mount: String,
    pub(crate) rows: Vec<Outcome>,
    pub(crate) dry_run: bool,
    pub(crate) plan: Plan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) revision: Option<String>,
}

impl MountRemoveReceipt {
    pub(crate) fn dry_run(mount: String, plan: Plan) -> Self {
        Self {
            verdict: Verdict::Ok,
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
        revision: Option<String>,
    ) -> Self {
        Self {
            verdict: Verdict::from_rows(&rows),
            mount,
            rows,
            dry_run: false,
            plan,
            revision,
        }
    }

    pub(crate) fn output_verdict(&self) -> ResultVerdict {
        self.verdict.output_verdict()
    }
}

impl TeardownReceipt {
    pub(crate) fn new(rows: Vec<Outcome>, detached: usize, still_attached: Vec<String>) -> Self {
        Self {
            verdict: Verdict::from_rows(&rows),
            rows,
            detached,
            still_attached,
        }
    }

    pub(crate) fn output_verdict(&self) -> ResultVerdict {
        self.verdict.output_verdict()
    }

    pub(crate) fn exit_code(&self) -> crate::error::ExitCode {
        match self.verdict {
            Verdict::Ok => crate::error::ExitCode::Success,
            Verdict::Degraded => crate::error::ExitCode::Degraded,
            Verdict::Failed => crate::error::ExitCode::GenericFailure,
        }
    }
}

/// `omnifs mount add`: the mount that was written and whether its
/// credential is live yet.
#[derive(Debug, Serialize)]
pub(crate) struct MountAddReceipt {
    pub(crate) verdict: Verdict,
    pub(crate) mount: String,
    pub(crate) status: MountInitStatus,
    pub(crate) revision: String,
}

/// `omnifs mount reauth`: the mount whose credential was refreshed.
#[derive(Debug, Serialize)]
pub(crate) struct MountReauthReceipt {
    pub(crate) verdict: Verdict,
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

        assert_eq!(receipt.verdict, Verdict::Failed);
        assert_eq!(receipt.output_verdict(), ResultVerdict::Degraded);
        assert_eq!(receipt.exit_code(), crate::error::ExitCode::GenericFailure);
        let json = serde_json::to_value(&receipt).unwrap();
        assert_eq!(json["detached"], 2);
        assert_eq!(json["still_attached"][0], "fuse/host at /mnt/omnifs");
    }
}
