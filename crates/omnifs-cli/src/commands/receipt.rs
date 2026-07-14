//! Typed structured receipts for the mutating and lifecycle commands.
//!
//! A receipt is the single terminal document a structured command emits on stdout
//! (Part 5 of the agent contract): typed structs, never hand-rolled `json!`,
//! with no human sentences inside values and a machine-visible `fix` on every
//! failed row. All narration stays on stderr. The commands own the side
//! effects; this module owns the wire shape they settle into.

use serde::Serialize;
use std::path::PathBuf;

use crate::commands::frontend::{FrontendEnvironment as Environment, FrontendId, FrontendResult};
use crate::inventory::{AccessPath, FrontendStatus, Inventory};
use crate::stages::MountInitStatus;
use crate::ui::consent::{Outcome, Plan};
use crate::ui::output::ResultVerdict;
use omnifs_workspace::mounts::Name as MountName;

/// The overall health of a completed operation. `up` reports `degraded` (and
/// exits 5) when any mount, frontend, or subsystem needs attention.
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
}

/// `omnifs up`: the daemon, its mounts and frontends, and a verdict.
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
}

impl MountRemoveReceipt {
    pub(crate) fn dry_run(mount: String, plan: Plan) -> Self {
        Self {
            verdict: Verdict::Ok,
            mount,
            rows: Vec::new(),
            dry_run: true,
            plan,
        }
    }

    pub(crate) fn applied(mount: String, plan: Plan, rows: Vec<Outcome>) -> Self {
        Self {
            verdict: Verdict::from_rows(&rows),
            mount,
            rows,
            dry_run: false,
            plan,
        }
    }

    pub(crate) fn output_verdict(&self) -> ResultVerdict {
        match self.verdict {
            Verdict::Ok => ResultVerdict::Ok,
            Verdict::Degraded | Verdict::Failed => ResultVerdict::Degraded,
        }
    }
}

impl TeardownReceipt {
    pub(crate) fn new(rows: Vec<Outcome>) -> Self {
        Self {
            verdict: Verdict::from_rows(&rows),
            rows,
        }
    }
}

/// `omnifs mount add`: the mount that was written and whether its
/// credential is live yet.
#[derive(Debug, Serialize)]
pub(crate) struct MountAddReceipt {
    pub(crate) verdict: Verdict,
    pub(crate) mount: String,
    pub(crate) status: MountAddStatus,
}

/// `omnifs mount reauth`: the mount whose credential was refreshed.
#[derive(Debug, Serialize)]
pub(crate) struct MountReauthReceipt {
    pub(crate) verdict: Verdict,
    pub(crate) mount: String,
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

/// The typed terminal receipt for frontend enable/disable/restart. The rows
/// retain operation outcomes, while `frontends` and `access_paths` are fresh
/// post-operation inventory facts rather than launch-time guesses.
#[derive(Debug, Serialize)]
pub(crate) struct FrontendReceipt {
    pub(crate) verdict: Verdict,
    pub(crate) changed: bool,
    pub(crate) rows: Vec<FrontendResult>,
    pub(crate) frontends: Vec<FrontendStatus>,
    pub(crate) access_paths: Vec<AccessPath>,
}

impl FrontendReceipt {
    pub(crate) fn from_inventory(inventory: &Inventory, rows: Vec<FrontendResult>) -> Self {
        let frontends = inventory
            .frontends
            .iter()
            .filter(|frontend| {
                rows.iter()
                    .any(|row| frontend_matches_id(frontend, &row.id))
            })
            .cloned()
            .collect::<Vec<_>>();
        let access_paths = frontend_access_paths(inventory, &rows);
        let failed = rows
            .iter()
            .any(|row| row.state == crate::commands::frontend::RuntimeState::Failed);
        let selected_degraded = frontends
            .iter()
            .any(|frontend| frontend.state.severity() >= crate::inventory::Severity::Attention);
        let verdict = if failed {
            Verdict::Failed
        } else if selected_degraded {
            Verdict::Degraded
        } else {
            Verdict::Ok
        };
        Self {
            verdict,
            changed: rows.iter().any(|row| row.changed),
            rows,
            frontends,
            access_paths,
        }
    }

    pub(crate) fn output_verdict(&self) -> ResultVerdict {
        match self.verdict {
            Verdict::Ok => ResultVerdict::Ok,
            Verdict::Degraded | Verdict::Failed => ResultVerdict::Degraded,
        }
    }

    pub(crate) fn exit_code(&self) -> crate::error::ExitCode {
        match self.verdict {
            Verdict::Ok => crate::error::ExitCode::Success,
            Verdict::Degraded | Verdict::Failed => crate::error::ExitCode::Degraded,
        }
    }
}

fn frontend_matches_id(frontend: &FrontendStatus, id: &FrontendId) -> bool {
    frontend.filesystem == id.filesystem()
        && frontend.environment == id.environment()
        && (id.environment() != Environment::Host || frontend.location.as_deref() == id.location())
}

fn frontend_access_paths(inventory: &Inventory, rows: &[FrontendResult]) -> Vec<AccessPath> {
    let mut paths = Vec::new();
    for mount in &inventory.mounts {
        let Ok(name) = MountName::new(mount.name.clone()) else {
            continue;
        };
        let root = mount.root.strip_prefix("/").unwrap_or(&mount.root);
        let expected = rows
            .iter()
            .filter_map(|row| {
                let location = row.id.location().map(PathBuf::from).or_else(|| {
                    (row.id.environment() != Environment::Host).then(|| PathBuf::from("/omnifs"))
                })?;
                Some((
                    row.id.filesystem(),
                    row.id.environment(),
                    location.join(root),
                ))
            })
            .collect::<Vec<_>>();
        paths.extend(inventory.access_paths(&name).into_iter().filter(|path| {
            expected.iter().any(|(filesystem, environment, location)| {
                path.filesystem == *filesystem
                    && path.environment == *environment
                    && path.path == *location
            })
        }));
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::frontend::RuntimeState;
    use crate::commands::frontend::{
        FrontendEnvironment as Environment, FrontendFilesystem as Filesystem, FrontendId,
    };
    use crate::inventory::{DaemonState, FrontendState};

    fn inventory_with_frontends(frontends: Vec<FrontendStatus>) -> Inventory {
        Inventory::test(DaemonState::Running, frontends, Vec::new())
    }

    #[test]
    fn frontend_receipt_ignores_unrelated_degraded_frontends() {
        let selected = FrontendId::new(
            Filesystem::Nfs,
            Environment::Host,
            Some(PathBuf::from("/mnt/omnifs")),
        );
        let unrelated = FrontendId::new(Filesystem::Fuse, Environment::Docker, None);
        let inventory = inventory_with_frontends(vec![
            FrontendStatus {
                filesystem: selected.filesystem(),
                environment: selected.environment(),
                location: selected.location().map(PathBuf::from),
                state: FrontendState::Attached,
                scope: "all",
                mount_count: 0,
                fix: None,
            },
            FrontendStatus {
                filesystem: unrelated.filesystem(),
                environment: unrelated.environment(),
                location: unrelated.location().map(PathBuf::from),
                state: FrontendState::Failed,
                scope: "all",
                mount_count: 0,
                fix: Some("omnifs up".into()),
            },
        ]);
        assert_eq!(inventory.verdict(), crate::inventory::Verdict::Degraded);

        let receipt = FrontendReceipt::from_inventory(
            &inventory,
            vec![FrontendResult {
                id: selected,
                state: RuntimeState::Attached,
                changed: true,
                fix: None,
                detail: None,
            }],
        );

        assert_eq!(receipt.verdict, Verdict::Ok);
        assert_eq!(receipt.exit_code(), crate::error::ExitCode::Success);
    }
}
