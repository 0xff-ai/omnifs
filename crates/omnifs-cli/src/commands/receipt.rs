//! Typed `--json` receipts for the mutating and lifecycle commands.
//!
//! A receipt is the single JSON document a `--json` command emits on stdout
//! (Part 5 of the agent contract): typed structs, never hand-rolled `json!`,
//! with no human sentences inside values and a machine-visible `fix` on every
//! failed row. All narration stays on stderr. The commands own the side
//! effects; this module owns the wire shape they settle into.

use serde::Serialize;

use crate::stages::MountInitStatus;
use crate::status::{FrontendJson, RuntimeJson, StatusJson, UserMountStatus};
use crate::ui::consent::Outcome;

/// The overall health of a completed operation. `up` reports `degraded` (and
/// exits 5) when any mount, frontend, or subsystem needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    Ok,
    Degraded,
    Failed,
}

/// `omnifs up --json`: the daemon, its mounts and frontends, and a verdict.
/// Reuses the same status view types so the receipt never drifts from
/// `omnifs status --json`.
#[derive(Debug, Serialize)]
pub(crate) struct UpReceipt {
    pub(crate) verdict: Verdict,
    pub(crate) daemon: RuntimeJson,
    pub(crate) mounts: Vec<UserMountStatus>,
    pub(crate) frontends: Vec<FrontendJson>,
}

impl UpReceipt {
    /// Build the receipt from a collected status view and its degraded verdict.
    pub(crate) fn from_status(status: StatusJson, degraded: bool) -> Self {
        Self {
            verdict: if degraded {
                Verdict::Degraded
            } else {
                Verdict::Ok
            },
            daemon: status.runtime,
            mounts: status.mounts,
            frontends: status.frontends,
        }
    }
}

/// `omnifs down --json` and `omnifs reset --json`: the settled operation rows
/// and a verdict. `Failed` marks a receipt whose exit code is non-zero even
/// though the document itself is the whole story (no separate error document).
#[derive(Debug, Serialize)]
pub(crate) struct TeardownReceipt {
    pub(crate) verdict: Verdict,
    pub(crate) rows: Vec<Outcome>,
}

impl TeardownReceipt {
    pub(crate) fn new(rows: Vec<Outcome>) -> Self {
        let failed = rows
            .iter()
            .any(|row| row.state == crate::ui::consent::OutcomeState::Fail);
        Self {
            verdict: if failed { Verdict::Failed } else { Verdict::Ok },
            rows,
        }
    }
}

/// `omnifs mount add --json`: the mount that was written and whether its
/// credential is live yet.
#[derive(Debug, Serialize)]
pub(crate) struct MountAddReceipt {
    pub(crate) verdict: Verdict,
    pub(crate) mount: String,
    pub(crate) status: MountAddStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MountAddStatus {
    /// The mount is authenticated and ready to serve.
    Ready,
    /// The spec was written but sign-in was declined; `mount reauth` completes
    /// it later.
    SignInDeclined,
}

impl From<MountInitStatus> for MountAddStatus {
    fn from(status: MountInitStatus) -> Self {
        match status {
            MountInitStatus::Ready => Self::Ready,
            MountInitStatus::SignInDeclined => Self::SignInDeclined,
        }
    }
}
