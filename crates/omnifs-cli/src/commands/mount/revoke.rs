//! `omnifs mount revoke` — explicitly remove one shared host credential.

use anyhow::{Context, anyhow};
use clap::Args;
use omnifs_auth::{OAuthClient, OAuthRevokeOutcome};
use omnifs_workspace::authn::AuthKind;
use omnifs_workspace::creds::{CredentialStore, FileStore};

use crate::auth::MountAuth;
use crate::stages::PromptMode;
use crate::ui::consent::{Decision, Outcome, Plan, Receipt, Row};
use crate::ui::output::Output;
use crate::workspace::Workspace;

#[derive(Args, Debug, Clone)]
pub struct RevokeArgs {
    /// Existing mount name whose configured credential should be revoked.
    pub name: String,
}

impl RevokeArgs {
    #[allow(clippy::too_many_lines)] // keep consent and upstream-before-local ordering linear
    pub(crate) async fn run(self, output: Output) -> anyhow::Result<Receipt> {
        let workspace = Workspace::resolve()?;
        output.intro(format!("omnifs mount revoke {}", self.name))?;
        let mounts = workspace.mounts()?;
        let auth = MountAuth::load(workspace.catalog(), &mounts, &self.name)?;
        let auth_config = auth
            .spec()
            .auth
            .as_ref()
            .ok_or_else(|| anyhow!("mount `{}` has no configured credential", self.name))?;
        let target = auth.credential_target()?;
        let credential_id = target
            .primary_key()
            .cloned()
            .ok_or_else(|| anyhow!("mount `{}` has no configured credential", self.name))?;

        let store = FileStore::new(workspace.layout().credentials_file.clone());
        let entry = store
            .get(&credential_id)
            .with_context(|| format!("read credential `{credential_id}`"))?;
        if let Some(entry) = entry.as_ref()
            && entry.kind() != auth_config.kind()
        {
            anyhow::bail!(
                "credential `{credential_id}` has kind {}, expected {}",
                entry.kind(),
                auth_config.kind()
            );
        }
        let oauth_request = if entry.is_some() && auth_config.kind() == AuthKind::OAuth {
            Some(auth.oauth_request(auth_config.account(), &[])?.0)
        } else {
            None
        };

        let mut affected_mounts = Vec::new();
        for mount in mounts {
            let candidate_auth = MountAuth::from_spec(workspace.catalog(), mount.config);
            let Some(candidate_config) = candidate_auth.spec().auth.as_ref() else {
                continue;
            };
            let mount_name = mount.name.to_string();
            let candidate = candidate_auth.credential_target()?;
            if candidate.primary_key() == Some(&credential_id) {
                if candidate_config.kind() != auth_config.kind() {
                    anyhow::bail!(
                        "mounts `{}` and `{mount_name}` share credential `{credential_id}` but configure different auth kinds",
                        self.name
                    );
                }
                if let Some(request) = oauth_request.as_ref() {
                    let candidate_request = candidate_auth
                        .oauth_request(candidate_config.account(), &[])?
                        .0;
                    if !request.has_same_runtime_metadata(&candidate_request) {
                        return Err(omnifs_auth::AuthError::CredentialBindingConflict {
                            id: credential_id.clone(),
                        }
                        .into());
                    }
                }
                affected_mounts.push(mount_name);
            }
        }
        debug_assert!(affected_mounts.iter().any(|mount| mount == &self.name));
        let affected_mounts = affected_mounts.join(", ");
        let label = format!("{credential_id} (used by mounts: {affected_mounts})");
        let mut plan = Plan::new("revoke");
        plan.push(if entry.is_some() {
            let action = oauth_request.as_ref().map_or("remove locally", |request| {
                if request.scheme().revocation_endpoint.is_some() {
                    "revoke upstream, then remove locally"
                } else {
                    "remove locally; provider declares no upstream revocation"
                }
            });
            Row::remove("credential", "credential", format!("{label}; {action}"))
        } else {
            Row::keep(
                "credential",
                "credential",
                format!("{label}; already absent"),
            )
        });
        output.plan(&plan);

        let Some(entry) = entry else {
            let receipt = plan.receipt([Outcome::skip(
                "credential",
                format!("already absent; used by mounts: {affected_mounts}"),
            )]);
            output.receipt(&receipt);
            output.outro(format!(
                "Credential `{credential_id}` is already absent; the running daemon was not changed."
            ));
            return Ok(receipt);
        };

        Decision::resolve(
            PromptMode::from_flags(output.yes(), output.no_input() || output.is_structured()),
            false,
            "--yes",
            &output,
        )?;

        let removal = if let Some(request) = oauth_request {
            if request.scheme().revocation_endpoint.is_none() {
                "removed locally; provider declares no upstream revocation"
            } else {
                match OAuthClient::new()
                    .context("create OAuth client for revocation")?
                    .revoke_access_token(request, entry.access_token().clone())
                    .await
                    .with_context(|| format!("revoke credential `{credential_id}` upstream"))?
                {
                    OAuthRevokeOutcome::Revoked => "revoked upstream and removed locally",
                    OAuthRevokeOutcome::Unsupported => {
                        "removed locally; provider declares no upstream revocation"
                    },
                }
            }
        } else {
            "removed locally"
        };

        store
            .delete(&credential_id)
            .with_context(|| format!("delete credential `{credential_id}`"))?;
        let receipt = plan.receipt([Outcome::done(
            "credential",
            format!("{removal}; used by mounts: {affected_mounts}"),
        )]);
        output.receipt(&receipt);
        output.outro(format!(
            "Credential `{credential_id}` removed; it applies on the next `omnifs up` or `omnifs apply`. The running daemon was not changed."
        ));
        Ok(receipt)
    }
}
