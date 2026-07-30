//! The shared mutation runner every mutating command goes through.
//!
//! One daemon-owned lease, one atomic batch, one journal record. `run` settles
//! whatever pending record a prior interrupted command left, acquires a fresh
//! lease, lets the caller build the batch from state read under that lease
//! (closing the TOCTOU gap between deciding what to write and writing it),
//! journals the batch before applying it, and clears the journal once the
//! daemon confirms the batch committed.

use crate::client_state::{ClientState, PendingMutation, PendingOp};
use crate::rpc::RpcClient;
use crate::ui::output::Output;
use anyhow::Context as _;
use omnifs_api::{CredentialKey, CredentialSubmission, MountDefinition, MountPatch, MutationOp};
use omnifs_core::{MountName, MutationId};
use std::future::Future;

/// One typed op paired with the journal label a settle-time provenance check
/// resolves it by. The label is derived once, at construction, from the same
/// facts that build the op itself, so the journal can never name a different
/// target than the batch actually touches.
pub(crate) struct PlannedOp {
    op: MutationOp,
    kind: &'static str,
    target: String,
}

impl PlannedOp {
    pub(crate) fn mount_create(definition: MountDefinition) -> Self {
        let target = definition.name.to_string();
        Self {
            op: MutationOp::CreateMount(definition),
            kind: "mount.create",
            target,
        }
    }

    pub(crate) fn mount_update(name: MountName, patch: MountPatch) -> Self {
        let target = name.to_string();
        Self {
            op: MutationOp::UpdateMount { name, patch },
            kind: "mount.update",
            target,
        }
    }

    pub(crate) fn mount_remove(name: MountName) -> Self {
        let target = name.to_string();
        Self {
            op: MutationOp::RemoveMount { name },
            kind: "mount.remove",
            target,
        }
    }

    pub(crate) fn submit_credential(key: &CredentialKey, submission: CredentialSubmission) -> Self {
        let target = credential_label(key);
        Self {
            op: MutationOp::SubmitCredential(submission),
            kind: "credential.submit",
            target,
        }
    }

    pub(crate) fn delete_credential(key: CredentialKey) -> Self {
        let target = credential_label(&key);
        Self {
            op: MutationOp::DeleteCredential(key),
            kind: "credential.delete",
            target,
        }
    }

    pub(crate) fn revoke_credential(key: CredentialKey) -> Self {
        let target = credential_label(&key);
        Self {
            op: MutationOp::RevokeCredential(key),
            kind: "credential.revoke",
            target,
        }
    }
}

fn credential_label(key: &CredentialKey) -> String {
    format!("{}:{}:{}", key.provider_name, key.scheme, key.account_label)
}

/// Result of a batch that actually reached `ApplyMutation`.
pub(crate) struct MutationOutcome {
    pub(crate) results: Vec<omnifs_api::MutationOpResult>,
    pub(crate) serving: omnifs_api::ServingOutcome,
}

/// Narrate a batch's serving outcome when the daemon's serving generation
/// does not yet reflect it. The batch itself already committed durably
/// (this only ever runs after a successful `ApplyMutation`); a consumer of
/// the mount right now may simply not see the change yet.
pub(crate) fn narrate_serving(output: &Output, serving: &omnifs_api::ServingOutcome) {
    if serving.serving {
        return;
    }
    let detail = serving
        .recovery_detail
        .as_deref()
        .unwrap_or("recovery required");
    output.narrate(format!(
        "the change committed, but the daemon is not yet serving it: {detail}"
    ));
}

/// Settle any pending mutation record left by an interrupted prior command:
/// re-read its targets and compare their `last_mutation_id` against the
/// journaled id (a create/update/submit/delete/revoke op leaves its target
/// present, so one match proves the whole atomic batch committed), or, for a
/// `mount.remove` op, treat the target's absence as settled either way (the
/// removal succeeded, or the mount was already gone). Always clears the
/// journal before returning, and narrates what it found.
pub(crate) async fn settle(
    rpc: &RpcClient,
    state: &ClientState,
    output: &Output,
) -> anyhow::Result<()> {
    let Some(pending) = state.pending()? else {
        return Ok(());
    };
    let committed = resolve_pending(rpc, &pending).await?;
    state.clear_pending()?;
    let summary = pending
        .ops
        .iter()
        .map(|op| format!("{} `{}`", op.kind, op.target))
        .collect::<Vec<_>>()
        .join(", ");
    if committed {
        output.narrate(format!(
            "a prior mutation ({summary}) had already committed before this client learned its outcome"
        ));
    } else {
        output.narrate(format!(
            "a prior mutation ({summary}) did not commit before this client lost track of it"
        ));
    }
    Ok(())
}

async fn resolve_pending(rpc: &RpcClient, pending: &PendingMutation) -> anyhow::Result<bool> {
    let mut saw_remove = false;
    let mut every_removed_target_absent = true;
    for op in &pending.ops {
        match op.kind.as_str() {
            "mount.create" | "mount.update" => {
                let name = MountName::new(op.target.clone())
                    .with_context(|| format!("parse pending mount target `{}`", op.target))?;
                if let Some(mount) = rpc.get_mount(name).await?
                    && mount.last_mutation_id == pending.id
                {
                    return Ok(true);
                }
            },
            "mount.remove" => {
                saw_remove = true;
                let name = MountName::new(op.target.clone())
                    .with_context(|| format!("parse pending mount target `{}`", op.target))?;
                if rpc.get_mount(name).await?.is_some() {
                    every_removed_target_absent = false;
                }
            },
            "credential.submit" | "credential.delete" | "credential.revoke" => {
                let key = parse_credential_target(&op.target)?;
                if let Some(status) = rpc.credential_status(key).await?
                    && status.last_mutation_id == pending.id
                {
                    return Ok(true);
                }
            },
            other => anyhow::bail!("pending mutation journal names an unknown op kind `{other}`"),
        }
    }
    Ok(saw_remove && every_removed_target_absent)
}

fn parse_credential_target(target: &str) -> anyhow::Result<CredentialKey> {
    let mut parts = target.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(provider_name), Some(scheme), Some(account_label)) => Ok(CredentialKey {
            provider_name: provider_name.to_owned(),
            scheme: scheme.to_owned(),
            account_label: account_label.to_owned(),
        }),
        _ => anyhow::bail!("pending mutation journal has a malformed credential target `{target}`"),
    }
}

/// Settle any stale journal, then run one fresh batch to completion: acquire
/// the lease, let `plan` build the ops (any reads it does happen under the
/// lease, closing the TOCTOU gap), journal them, apply, and clear the
/// journal. `plan` returning no ops means nothing needs to change; the fresh
/// lease is dropped and this returns `Ok(None)`.
///
/// A transport error from `ApplyMutation` leaves the journal in place (the
/// next command through here settles it) and propagates as-is; the daemon's
/// `MutationInProgress`/`LeaseExpired`/`LeaseNotHeld` errors already carry an
/// actionable message (holder id and lease deadline, where relevant), so
/// they also propagate unchanged rather than being retried automatically.
pub(crate) async fn run<F, Fut>(
    rpc: &RpcClient,
    state: &ClientState,
    output: &Output,
    plan: F,
) -> anyhow::Result<Option<MutationOutcome>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<Vec<PlannedOp>>>,
{
    settle(rpc, state, output).await?;
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generate mutation id")?;
    let id = MutationId::from_bytes(bytes);
    rpc.begin_mutation(id).await?;

    let planned = match plan().await {
        Ok(planned) => planned,
        Err(error) => {
            // Nothing was journaled yet, so the lease is simply abandoned;
            // dropping it lets the next `begin` reclaim it without waiting
            // out the full lease.
            let _ = rpc.drop_mutation(id).await;
            return Err(error);
        },
    };
    if planned.is_empty() {
        rpc.drop_mutation(id).await?;
        return Ok(None);
    }

    let mut ops = Vec::with_capacity(planned.len());
    let mut journal_ops = Vec::with_capacity(planned.len());
    for op in planned {
        journal_ops.push(PendingOp {
            kind: op.kind.to_owned(),
            target: op.target,
        });
        ops.push(op.op);
    }
    state.set_pending(&PendingMutation {
        id,
        ops: journal_ops,
    })?;

    let (results, serving) = rpc.apply_mutation(id, ops).await?;
    state.clear_pending()?;
    Ok(Some(MutationOutcome { results, serving }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planned_ops_derive_their_own_journal_labels() {
        let mount_name = MountName::new("github").unwrap();
        let create = PlannedOp::mount_create(MountDefinition {
            name: mount_name.clone(),
            provider: omnifs_core::ProviderId::from_wasm_bytes(b"demo"),
            auth: None,
            limits: None,
            config: b"{}".to_vec(),
        });
        assert_eq!(create.kind, "mount.create");
        assert_eq!(create.target, "github");

        let remove = PlannedOp::mount_remove(mount_name);
        assert_eq!(remove.kind, "mount.remove");
        assert_eq!(remove.target, "github");

        let key = CredentialKey {
            provider_name: "github".to_owned(),
            scheme: "oauth".to_owned(),
            account_label: "work".to_owned(),
        };
        let revoke = PlannedOp::revoke_credential(key);
        assert_eq!(revoke.kind, "credential.revoke");
        assert_eq!(revoke.target, "github:oauth:work");
    }

    #[test]
    fn credential_target_parses_back_into_its_key() {
        let key = CredentialKey {
            provider_name: "github".to_owned(),
            scheme: "oauth".to_owned(),
            account_label: "work".to_owned(),
        };
        let label = credential_label(&key);
        let parsed = parse_credential_target(&label).unwrap();
        assert_eq!(parsed, key);
    }

    #[test]
    fn credential_target_keeps_a_colon_inside_the_account_label() {
        let key = CredentialKey {
            provider_name: "github".to_owned(),
            scheme: "oauth".to_owned(),
            account_label: "work:secondary".to_owned(),
        };
        let parsed = parse_credential_target(&credential_label(&key)).unwrap();
        assert_eq!(parsed.account_label, "work:secondary");
    }

    #[test]
    fn malformed_credential_target_is_rejected() {
        assert!(parse_credential_target("github:oauth").is_err());
        assert!(parse_credential_target("").is_err());
    }
}
