//! `omnifs reset`: plan and apply a complete workspace cleanup.
//!
//! Reset is intentionally a consent session even when `--yes` is supplied:
//! the plan is always visible, and the receipt settles the same row identities.
//! `--dry-run` stops after the plan. Credential revocation remains upstream
//! best-effort, but a local credential-store delete failure aborts before its
//! mount spec is touched.

use anyhow::Context;
use clap::Args;
use omnifs_auth::{CredentialService, OAuthClient};
use omnifs_workspace::creds::{CredentialStore, FileStore};
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::runtime_record::RuntimeRecord;
use std::sync::Arc;

use crate::commands::mounts::delete_credentials;
use crate::credential_target::CredentialTarget;
use crate::daemon_teardown::DaemonTeardown;
use crate::stages::PromptMode;
use crate::ui::consent::{Decision, Outcome, Plan, Row};
use crate::workspace::{MountRemovalTarget, Workspace};

#[derive(Args, Debug, Clone, Default)]
pub struct ResetArgs {
    /// Skip the confirmation prompt. The plan and receipt are still shown.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Keep stored credentials; only delete mount configs and the daemon.
    #[arg(long)]
    pub keep_credentials: bool,
    /// Print the reset plan and make no changes.
    #[arg(long)]
    pub dry_run: bool,
}

impl ResetArgs {
    #[allow(clippy::too_many_lines)] // plan/apply/receipt is one auditable flow
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let layout = workspace.layout();
        let targets = workspace.reset_removal_targets()?;
        let mut session = crate::ui::session::Session::intro("omnifs reset")?;
        let plan = reset_plan(&workspace, &targets, self.keep_credentials);
        session.plan(&plan);

        let decision =
            Decision::resolve(PromptMode::from_flags(self.yes, false), self.dry_run, "-y")?;
        match decision {
            Decision::Decline => {
                session.outro("Reset aborted.");
                return Ok(());
            },
            Decision::DryRun => {
                session.outro("Dry run; no changes made.");
                return Ok(());
            },
            Decision::Apply => {},
        }

        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::new(&layout.credentials_file));
        let service = CredentialService::new(store, OAuthClient::new()?);
        let mut outcomes = Vec::with_capacity(plan.rows.len());

        for target in &targets {
            let mut credential_outcomes = Vec::new();
            if !self.keep_credentials {
                let credential = target
                    .config
                    .as_ref()
                    .map(|spec| {
                        crate::auth::MountAuth::from_spec(workspace.catalog(), spec.clone())
                            .register_revocation(&service)
                    })
                    .transpose()?
                    .unwrap_or_else(|| target.credential.clone());
                credential_outcomes = delete_credentials(&service, &credential).await;
            }

            let mount_id = format!("mount:{}", target.name);
            let mut mount_outcome = if let Some(failure) = credential_outcomes
                .iter()
                .find(|outcome| outcome.state == crate::ui::consent::OutcomeState::Fail)
            {
                Outcome::fail(
                    &mount_id,
                    format!("spec kept; credential deletion failed: {}", failure.value),
                )
            } else {
                match workspace.daemon().delete_mount_if_ready(&target.name).await {
                    Ok(Some(report)) if report.failure.is_none() => {
                        Outcome::done(&mount_id, "spec deleted (hot unload from running daemon)")
                    },
                    Ok(Some(report)) => {
                        let reason = report
                            .failure
                            .as_ref()
                            .map_or("unknown daemon error", |failure| failure.reason.as_str());
                        Outcome::warn(
                            &mount_id,
                            format!("spec deleted; hot unload failed ({reason})"),
                        )
                    },
                    Ok(None) => match remove_mount_locally(&workspace, target) {
                        Ok(true) => {
                            Outcome::done(&mount_id, "spec deleted (cold delete; daemon stopped)")
                        },
                        Ok(false) => Outcome::skip(&mount_id, "spec already absent (cold delete)"),
                        Err(error) => Outcome::fail(
                            &mount_id,
                            format!("spec kept; local delete failed: {error:#}"),
                        ),
                    },
                    Err(error) => match remove_mount_locally(&workspace, target) {
                        Ok(true) => Outcome::warn(
                            &mount_id,
                            format!("deleted (cold delete; hot unload unavailable: {error:#})"),
                        ),
                        Ok(false) => Outcome::skip(
                            &mount_id,
                            format!(
                                "already absent (cold delete; hot unload unavailable: {error:#})"
                            ),
                        ),
                        Err(local_error) => Outcome::fail(
                            &mount_id,
                            format!(
                                "spec kept; hot unload failed ({error:#}); local delete failed: {local_error:#}"
                            ),
                        ),
                    },
                }
            };
            for credential in credential_outcomes {
                mount_outcome = mount_outcome.with_detail(credential);
            }
            outcomes.push(mount_outcome);
        }

        let mut teardown_outcomes: Vec<Outcome> = Vec::new();
        let planned_frontends = plan
            .rows
            .iter()
            .filter(|row| row.id.starts_with("frontend:") || row.id == "frontends")
            .cloned()
            .collect::<Vec<_>>();
        for teardown in DaemonTeardown::new(&workspace).reset_best_effort().await {
            // Runtime-record cleanup is a detail of daemon teardown. Keeping it
            // nested avoids inventing a plan row for an internal file.
            if teardown.id() == "runtime-record" {
                if let Some(daemon) = teardown_outcomes
                    .iter_mut()
                    .find(|outcome| outcome.id == "daemon")
                {
                    *daemon = daemon.clone().with_detail(teardown.outcome());
                } else {
                    teardown_outcomes.push(Outcome::skip("daemon", teardown.outcome().value));
                }
            } else if teardown.id() == "frontends"
                && planned_frontends.iter().any(|row| row.id != "frontends")
            {
                let outcome = teardown.outcome();
                for planned in &planned_frontends {
                    if planned.id == "frontends" {
                        continue;
                    }
                    teardown_outcomes.push(
                        outcome
                            .clone()
                            .with_id(planned.id.clone())
                            .with_key(planned.key.clone()),
                    );
                }
            } else {
                teardown_outcomes.push(teardown.outcome());
            }
        }
        outcomes.extend(teardown_outcomes);
        outcomes.push(Outcome::skip(
            "provider-store",
            format!("kept ({})", provider_summary(&workspace)),
        ));

        let receipt = plan.receipt(outcomes);
        session.receipt(&receipt);
        let failed = receipt
            .rows
            .iter()
            .find(|row| row.state == crate::ui::consent::OutcomeState::Fail)
            .map(|row| row.value.clone());
        if let Some(failed) = failed {
            session.outro("Reset incomplete; see the failed rows above.");
            anyhow::bail!(failed);
        } else if targets.is_empty() {
            session.outro("Reset complete; no mounts were configured.");
        } else {
            session.outro("Reset complete. Run `omnifs setup` to start again.");
        }
        crate::telemetry::maybe_print_health_nudge(&workspace).await;
        Ok(())
    }
}

fn reset_plan(
    workspace: &Workspace,
    targets: &[MountRemovalTarget],
    keep_credentials: bool,
) -> Plan {
    let mut plan = Plan::new("plan");
    plan.rows.extend(planned_frontend_rows(workspace));
    plan.push(Row::remove("daemon", "daemon", "stop if running"));
    for target in targets {
        let credential = match (&target.credential, keep_credentials) {
            (CredentialTarget::Internal(_), false) => " + revoke credential",
            (CredentialTarget::Internal(_), true) => " (keep credential)",
            (CredentialTarget::None, _) => "",
        };
        plan.push(Row::remove(
            format!("mount:{}", target.name),
            format!("mount {}", target.name),
            format!(
                "delete {}{credential}",
                WorkspaceLayout::display(&target.path)
            ),
        ));
    }
    plan.push(Row::keep(
        "provider-store",
        "provider",
        format!("store kept ({})", provider_summary(workspace)),
    ));
    plan
}

fn planned_frontend_rows(workspace: &Workspace) -> Vec<Row> {
    RuntimeRecord::read(&workspace.layout().runtime_record_file())
        .ok()
        .flatten()
        .map(|record| {
            record
                .frontends
                .into_iter()
                .map(|frontend| {
                    let id = format!(
                        "frontend:{}:{}:{}",
                        frontend.via.label(),
                        frontend.kind.label(),
                        frontend.mount_point.display()
                    );
                    Row::remove(
                        id,
                        format!(
                            "frontend {} ({})",
                            frontend.kind.label(),
                            frontend.via.label()
                        ),
                        format!(
                            "tear down {}",
                            WorkspaceLayout::display(&frontend.mount_point)
                        ),
                    )
                })
                .collect::<Vec<_>>()
        })
        .filter(|rows| !rows.is_empty())
        .unwrap_or_else(|| {
            vec![Row::remove(
                "frontends",
                "frontends",
                "tear down running frontends",
            )]
        })
}

fn provider_summary(workspace: &Workspace) -> String {
    let providers = workspace
        .catalog()
        .installable()
        .map_or(0, |providers| providers.len());
    let artifacts = workspace
        .catalog()
        .installed()
        .map_or(0, |artifacts| artifacts.len());
    format!("{providers} providers, {artifacts} artifacts")
}

fn remove_mount_locally(
    workspace: &Workspace,
    target: &MountRemovalTarget,
) -> anyhow::Result<bool> {
    let name = omnifs_workspace::mounts::Name::new(target.name.clone())
        .with_context(|| format!("invalid mount name `{}`", target.name))?;
    workspace
        .remove_mount(&name)
        .with_context(|| format!("remove {}", target.path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixture_paths;

    #[test]
    fn reset_plan_counts_frontends_daemon_mounts_and_keeps_provider_store() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = fixture_paths(tmp.path());
        std::fs::create_dir_all(&paths.mounts_dir).unwrap();
        let workspace = Workspace::from_layout(paths);
        let plan = reset_plan(&workspace, &[], false);
        assert_eq!(plan.remove_count(), 2);
        assert_eq!(plan.keep_count(), 1);
        assert_eq!(
            plan.rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["frontends", "daemon", "provider-store"]
        );
    }
}
