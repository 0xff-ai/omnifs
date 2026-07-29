//! `omnifs mount revoke` revokes one daemon-owned credential upstream.

use anyhow::anyhow;
use clap::Args;
use omnifs_api::{CredentialKey, CredentialStatusKind};

use crate::client_state::ClientState;
use crate::mutation::PlannedOp;
use crate::ui::consent::{Decision, Outcome, Plan, Receipt, Row};
use crate::ui::output::Output;

#[derive(Args, Debug, Clone)]
pub struct RevokeArgs {
    /// Existing mount name whose configured credential should be revoked.
    pub name: String,
}

impl RevokeArgs {
    pub(crate) async fn run(self, output: Output) -> anyhow::Result<Receipt> {
        crate::commands::daemon_start::start(&output).await?;
        let rpc = crate::rpc::RpcClient::resolve()?;
        let name = omnifs_core::MountName::new(self.name.clone())?;
        let requested = rpc
            .get_mount(name.clone())
            .await?
            .ok_or_else(|| anyhow!("no mount named `{}`", self.name))?;
        let auth = requested
            .definition
            .auth
            .as_ref()
            .ok_or_else(|| anyhow!("mount `{}` has no configured credential", self.name))?;
        let key = CredentialKey {
            provider_name: requested.provider.name.clone(),
            scheme: auth.scheme.clone(),
            account_label: auth.account_label.clone(),
        };
        let status = rpc.credential_status(key.clone()).await?;
        let mounts = rpc.list_mounts().await?;
        let affected = mounts
            .iter()
            .filter_map(|mount| {
                let candidate = mount.definition.auth.as_ref()?;
                (mount.provider.name == requested.provider.name
                    && candidate.scheme == auth.scheme
                    && candidate.account_label == auth.account_label)
                    .then(|| mount.definition.name.to_string())
            })
            .collect::<Vec<_>>();
        let affected_label = affected.join(", ");
        let mut plan = Plan::new(format!("Revoking credential for `{}`", self.name));
        if status
            .as_ref()
            .is_some_and(|status| status.status != CredentialStatusKind::Deleted)
        {
            plan.push(Row::remove(
                "credential",
                "credential",
                format!(
                    "{}/{} (used by mounts: {affected_label})",
                    auth.scheme, auth.account_label
                ),
            ));
        } else {
            plan.push(Row::keep(
                "credential",
                "credential",
                format!("{}/{} (already absent)", auth.scheme, auth.account_label),
            ));
        }
        output.plan(&plan);
        let Some(status) = status else {
            let receipt = plan.receipt([Outcome::skip("credential", "already absent")]);
            output.receipt(&receipt);
            output.outro(format!("Credential for `{}` is already absent.", self.name));
            return Ok(receipt);
        };
        if status.status == CredentialStatusKind::Deleted {
            let receipt = plan.receipt([Outcome::skip("credential", "already absent")]);
            output.receipt(&receipt);
            output.outro(format!("Credential for `{}` is already absent.", self.name));
            return Ok(receipt);
        }
        Decision::resolve(output.prompt_mode(), false, "Revoke?", "--yes", &output)?;

        let state = ClientState::resolve()?;
        if let Some(outcome) = crate::mutation::run(&rpc, &state, &output, || async move {
            Ok(vec![PlannedOp::revoke_credential(key)])
        })
        .await?
        {
            crate::mutation::narrate_serving(&output, &outcome.serving);
        }

        let receipt = plan.receipt([Outcome::done(
            "credential",
            format!("revoked; used by mounts: {affected_label}"),
        )]);
        output.receipt(&receipt);
        output.outro(format!("Credential for `{}` revoked.", self.name));
        Ok(receipt)
    }
}
