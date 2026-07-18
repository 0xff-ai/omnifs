//! `omnifs up`: daemon lifecycle start.

use std::time::Duration;

use clap::Args;

use crate::commands::receipt::UpReceipt;
use crate::error::ExitCode;
use crate::inventory::Inventory;
use crate::launch::{LaunchOutcome, Launcher};
use crate::ui::output::{Output, ResultVerdict};
use omnifs_workspace::Workspace;

#[derive(Args, Debug, Clone, Default)]
pub struct UpArgs {
    /// Maximum time to wait for daemon readiness, failing with exit code 3 on timeout.
    #[arg(long, value_name = "DURATION")]
    pub wait: Option<String>,
    /// Start a cache-only daemon from the exact current mount revision.
    #[arg(long)]
    pub offline: bool,
}

impl UpArgs {
    pub(crate) async fn start_in_workspace(
        &self,
        workspace: &Workspace,
        output: Output,
    ) -> anyhow::Result<LaunchOutcome> {
        let wait = self
            .wait
            .as_deref()
            .map(crate::stages::parse_wait_duration)
            .transpose()?
            .unwrap_or(Duration::from_mins(2));
        let outcome = Launcher::new(workspace, "omnifs up", output.clone(), self.offline, wait)
            .launch()
            .await?;

        if self.wait.is_some() && !output.is_structured() {
            output.narrate("Daemon is ready.");
        }
        Ok(outcome)
    }

    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        let workspace = Workspace::resolve()?;
        let outcome = self.start_in_workspace(&workspace, output.clone()).await?;
        crate::metrics::maybe_print_health_nudge(&workspace, output.clone()).await;

        if output.is_structured() {
            return emit_receipt(&workspace, output).await;
        }

        // The closing line is the single most actionable next thing: a no-op
        // collapses to one "already serving" sentence naming
        // only the primary surface; a real start prints the full access
        // block below the ledger rows `Launcher` already printed. The daemon
        // is already up at this point, so a failure to read the inventory
        // only costs the closing summary, never the command's success.
        match Inventory::collect(&workspace).await {
            Ok(inventory) => match outcome {
                LaunchOutcome::AlreadyServing => output.outro(no_op_message(&inventory)),
                LaunchOutcome::Started => {
                    output.narrate("");
                    for line in crate::ui::access::lines(&inventory) {
                        output.narrate(line);
                    }
                },
            },
            Err(error) => output.note(format!(
                "daemon is up, but the closing summary is unavailable: {error:#}"
            )),
        }
        Ok(ExitCode::Success)
    }
}

/// `Already serving revision <sha>. Files at <location>`, or a
/// shorter fallback when there is no revision yet or no attached host
/// frontend to name.
fn no_op_message(inventory: &Inventory) -> String {
    let revision = inventory
        .applied_revision
        .as_ref()
        .or(inventory.mount_revision.as_ref());
    let location =
        crate::ui::access::primary_host_location(inventory).map(omnifs_workspace::display);
    match (revision, location) {
        (Some(revision), Some(location)) => {
            format!("Already serving revision {revision}. Files at {location}")
        },
        (Some(revision), None) => format!("Already serving revision {revision}."),
        (None, _) => "Already serving.".to_owned(),
    }
}

/// Collect the post-launch status and emit the `up` receipt. The verdict and
/// exit code come from the same inventory degraded check that
/// `omnifs status` uses, so a degraded daemon exits 5 here too.
async fn emit_receipt(workspace: &Workspace, output: Output) -> anyhow::Result<ExitCode> {
    let inventory = Inventory::collect(workspace).await?;
    let degraded = inventory.verdict() == crate::inventory::Verdict::Degraded;
    let verdict = if degraded {
        ResultVerdict::Degraded
    } else {
        ResultVerdict::Ok
    };
    output.emit_result(verdict, UpReceipt::from_inventory(inventory))?;
    Ok(if degraded {
        ExitCode::Degraded
    } else {
        ExitCode::Success
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::frontend::{FrontendFilesystem as Filesystem, FrontendRuntime as Runtime};
    use crate::inventory::{DaemonState, FrontendState, FrontendStatus};
    use omnifs_workspace::mounts::Revision;
    use std::path::PathBuf;

    /// the no-op line: `Already serving revision 3f69473. Files at
    /// ~/omnifs`.
    #[test]
    fn no_op_message_names_the_revision_and_the_primary_host_surface() {
        let mut inventory = Inventory::test(DaemonState::Running, Vec::new(), Vec::new());
        inventory.applied_revision = Some(Revision::new("3".repeat(40)).unwrap());
        // A path outside any real $HOME, so `omnifs_workspace::display`'s
        // `~`-collapse never fires and the assertion stays independent of
        // the test machine's environment.
        inventory.frontends.push(FrontendStatus {
            filesystem: Filesystem::Nfs,
            runtime: Runtime::Host,
            location: Some(PathBuf::from("/mnt/omnifs-test-home/omnifs")),
            state: FrontendState::Attached,
            scope: "all",
            mount_count: 0,
            fix: None,
        });
        assert_eq!(
            no_op_message(&inventory),
            format!(
                "Already serving revision {}. Files at /mnt/omnifs-test-home/omnifs",
                "3".repeat(40)
            )
        );
    }

    #[test]
    fn no_op_message_degrades_gracefully_without_a_revision_or_a_host_frontend() {
        let inventory = Inventory::test(DaemonState::Running, Vec::new(), Vec::new());
        assert_eq!(no_op_message(&inventory), "Already serving.");
    }

    #[test]
    fn no_op_message_still_names_the_revision_without_an_attached_host_frontend() {
        let mut inventory = Inventory::test(DaemonState::Running, Vec::new(), Vec::new());
        inventory.applied_revision = Some(Revision::new("4".repeat(40)).unwrap());
        assert_eq!(
            no_op_message(&inventory),
            format!("Already serving revision {}.", "4".repeat(40))
        );
    }
}
