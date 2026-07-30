//! `omnifs credential` lists and removes daemon-owned credentials.

use anyhow::Context as _;
use clap::{Args, Subcommand};
use omnifs_api::{
    CredentialKey, CredentialKind, CredentialStatus, CredentialStatusKind, MutationOpResult,
};
use serde::Serialize;

use crate::client_state::ClientState;
use crate::error::ExitCode;
use crate::mutation::PlannedOp;
use crate::ui::consent::{Decision, Outcome, Plan, Receipt, Row};
use crate::ui::output::{Output, ResultVerdict};

#[derive(Args, Debug, Clone)]
pub struct CredentialArgs {
    #[command(subcommand)]
    pub command: CredentialCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum CredentialCommand {
    /// List stored credentials without exposing their material.
    Ls,
    /// Delete stored credential material without revoking it upstream.
    Rm(RemoveArgs),
}

#[derive(Args, Debug, Clone)]
pub struct RemoveArgs {
    /// Provider name in the credential key.
    #[arg(long)]
    pub provider: String,
    /// Provider auth scheme in the credential key.
    #[arg(long)]
    pub scheme: String,
    /// Account label in the credential key.
    #[arg(long)]
    pub account: String,
}

impl CredentialArgs {
    pub async fn run(self, output: Output) -> anyhow::Result<ExitCode> {
        match self.command {
            CredentialCommand::Ls => ls(output).await,
            CredentialCommand::Rm(args) => {
                let receipt = args.run(output.clone()).await?;
                if output.is_structured() {
                    output.emit_result(ResultVerdict::Ok, receipt)?;
                }
                Ok(ExitCode::Success)
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct CredentialsResult {
    credentials: Vec<CredentialStatus>,
}

async fn ls(output: Output) -> anyhow::Result<ExitCode> {
    let credentials = crate::rpc::RpcClient::resolve()?.list_credentials().await?;
    let result = CredentialsResult { credentials };
    if output.is_structured() {
        output.emit_result(ResultVerdict::Ok, result)?;
    } else {
        output.report(render_credentials(&result));
    }
    Ok(ExitCode::Success)
}

impl RemoveArgs {
    async fn run(self, output: Output) -> anyhow::Result<Receipt> {
        crate::commands::daemon_start::start(&output).await?;
        let rpc = crate::rpc::RpcClient::resolve()?;
        let removal = CredentialRemoval::load(self, &rpc, &output).await?;
        let state = ClientState::resolve()?;

        if removal.is_absent() {
            // The read above already proves this credential absent; settle
            // whatever a prior interrupted command left in the journal (a
            // `credential.delete` batch stamps the row it tombstones, so the
            // absence check here is unrelated to that provenance check, but
            // hygiene still wants the journal cleared).
            crate::mutation::settle(&rpc, &state, &output).await?;
            return Ok(removal.skip(&output));
        }
        Decision::resolve(
            output.prompt_mode(),
            false,
            "Delete stored credential?",
            "--yes",
            &output,
        )?;
        removal.apply(&rpc, &state, &output).await?;
        Ok(removal.finish(&output))
    }
}

struct CredentialRemoval {
    key: CredentialKey,
    status: Option<CredentialStatus>,
    label: String,
    plan: Plan,
}

impl CredentialRemoval {
    async fn load(
        args: RemoveArgs,
        rpc: &crate::rpc::RpcClient,
        output: &Output,
    ) -> anyhow::Result<Self> {
        let key = CredentialKey {
            provider_name: args.provider,
            scheme: args.scheme,
            account_label: args.account,
        };
        let status = rpc.credential_status(key.clone()).await?;
        let mounts = rpc.list_mounts().await?;
        let affected = mounts
            .iter()
            .filter_map(|mount| {
                let auth = mount.definition.auth.as_ref()?;
                (mount.provider.name == key.provider_name
                    && auth.scheme == key.scheme
                    && auth.account_label == key.account_label)
                    .then(|| mount.definition.name.to_string())
            })
            .collect::<Vec<_>>();
        let affected_label = if affected.is_empty() {
            "not currently used by a mount".to_owned()
        } else {
            format!("currently used by mounts: {}", affected.join(", "))
        };
        let label = format!("{}/{}/{}", key.provider_name, key.scheme, key.account_label);
        let mut plan = Plan::new(format!("Deleting stored credential `{label}`"));
        if status
            .as_ref()
            .is_some_and(|status| status.status != CredentialStatusKind::Deleted)
        {
            plan.push(Row::remove(
                "credential",
                "credential",
                format!("{label} ({affected_label})"),
            ));
        } else {
            plan.push(Row::keep(
                "credential",
                "credential",
                format!("{label} (already absent)"),
            ));
        }
        output
            .narrate("This deletes local credential material. It does not revoke access upstream.");
        output.plan(&plan);

        Ok(Self {
            key,
            status,
            label,
            plan,
        })
    }

    fn is_absent(&self) -> bool {
        self.status
            .as_ref()
            .is_none_or(|status| status.status == CredentialStatusKind::Deleted)
    }

    async fn apply(
        &self,
        rpc: &crate::rpc::RpcClient,
        state: &ClientState,
        output: &Output,
    ) -> anyhow::Result<()> {
        let key = self.key.clone();
        let outcome = crate::mutation::run(rpc, state, output, || async move {
            Ok(vec![PlannedOp::delete_credential(key)])
        })
        .await?
        .context("credential deletion produced no result")?;
        crate::mutation::narrate_serving(output, &outcome.serving);
        let status = outcome
            .results
            .into_iter()
            .find_map(|result| match result {
                MutationOpResult::Credential(status) => Some(status),
                MutationOpResult::Mount(_) => None,
            })
            .context("credential deletion batch did not include a credential result")?;
        anyhow::ensure!(
            status.status == CredentialStatusKind::Deleted,
            "daemon did not mark the credential deleted"
        );
        Ok(())
    }

    fn skip(&self, output: &Output) -> Receipt {
        let receipt = self
            .plan
            .receipt([Outcome::skip("credential", "already absent")]);
        output.receipt(&receipt);
        output.outro(format!("Credential `{}` is already absent.", self.label));
        receipt
    }

    fn finish(&self, output: &Output) -> Receipt {
        let receipt = self.plan.receipt([Outcome::done(
            "credential",
            "deleted; upstream access not revoked",
        )]);
        output.receipt(&receipt);
        output.outro(format!(
            "Deleted stored credential `{}`. Upstream access was not revoked.",
            self.label
        ));
        receipt
    }
}

fn render_credentials(result: &CredentialsResult) -> String {
    use crate::ui::table::{
        Block, Cell, Column, Priority, Report, ResourceRow, ResourceTable, WidthPolicy,
    };

    let mut table = ResourceTable::new(
        "Credentials",
        format!("{} records", result.credentials.len()),
        vec![
            Column::new("Provider", Priority::Identity, WidthPolicy::Auto),
            Column::new("Scheme", Priority::Identity, WidthPolicy::Auto),
            Column::new("Account", Priority::Essential, WidthPolicy::Auto),
            Column::new("Kind", Priority::Secondary, WidthPolicy::Auto),
            Column::new("Scopes", Priority::Detail, WidthPolicy::Auto),
            Column::new("Version", Priority::Secondary, WidthPolicy::Auto),
            Column::new("Status", Priority::Essential, WidthPolicy::Auto),
        ],
    );
    for credential in &result.credentials {
        let state = credential_state(credential.status);
        table.push(ResourceRow::new(
            [
                Cell::new(&credential.key.provider_name),
                Cell::new(&credential.key.scheme),
                Cell::new(&credential.key.account_label),
                Cell::new(credential_kind(credential.kind)),
                Cell::new(if credential.scopes.is_empty() {
                    "none".to_owned()
                } else {
                    credential.scopes.join(", ")
                }),
                Cell::new(credential.version.get().to_string()),
                Cell::state(state.clone()),
            ],
            state,
        ));
    }
    let mut report = Report::new();
    report.push(Block::Resources(table));
    report.render()
}

const fn credential_kind(kind: CredentialKind) -> &'static str {
    match kind {
        CredentialKind::StaticToken => "static token",
        CredentialKind::OAuth => "oauth",
    }
}

fn credential_state(status: CredentialStatusKind) -> crate::ui::table::StateToken {
    use crate::ui::table::StateToken;

    match status {
        CredentialStatusKind::Active => StateToken::positive("active"),
        CredentialStatusKind::Blocked => StateToken::failure("blocked"),
        CredentialStatusKind::PendingRepublish => StateToken::attention("pending republish"),
        CredentialStatusKind::RevocationPending => StateToken::attention("revocation pending"),
        CredentialStatusKind::RevocationUnknown => StateToken::failure("revocation unknown"),
        CredentialStatusKind::Deleted => StateToken::neutral("deleted"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_credential_state_has_a_distinct_human_token() {
        let tokens = [
            (CredentialStatusKind::Active, "● active"),
            (CredentialStatusKind::Blocked, "× blocked"),
            (
                CredentialStatusKind::PendingRepublish,
                "▲ pending republish",
            ),
            (
                CredentialStatusKind::RevocationPending,
                "▲ revocation pending",
            ),
            (
                CredentialStatusKind::RevocationUnknown,
                "× revocation unknown",
            ),
            (CredentialStatusKind::Deleted, "○ deleted"),
        ];

        for (status, expected) in tokens {
            assert_eq!(credential_state(status).render(false), expected);
        }
    }
}
