//! The single entry point for the six wire mutation ops.
//!
//! `apply_batch` runs every op in one transaction: the first failure rolls
//! back everything already applied in the same call, so an interrupted batch
//! never leaves a partial write observable. Every mounts/credentials row the
//! batch creates or updates is stamped with the batch's `MutationId`, and the
//! global mount revision advances at most once per batch, regardless of how
//! many mount ops it carries.

use omnifs_auth::CredentialId;
use omnifs_core::{MountName, MountRevision, MutationId};

use crate::credential::CredentialDocument;
use crate::db::Db;
use crate::mount::MountDocument;
use crate::{
    CredentialMutationOutcome, CredentialWriteError, MountMutationOutcome, MountWriteError,
};

/// One typed state change, mirroring the six wire mutation ops one-to-one.
pub enum StateOp {
    CreateMount(MountDocument),
    UpdateMount(MountDocument),
    RemoveMount(MountName),
    SubmitCredential(CredentialDocument),
    DeleteCredential(CredentialId),
    RevokeCredential {
        id: CredentialId,
        scopes: Vec<String>,
    },
}

impl StateOp {
    const fn touches_mounts(&self) -> bool {
        matches!(
            self,
            Self::CreateMount(_) | Self::UpdateMount(_) | Self::RemoveMount(_)
        )
    }
}

/// The per-op result a batch collects, in submitted order.
#[derive(Debug, Clone, PartialEq)]
pub enum OpOutcome {
    Mount(MountMutationOutcome),
    Credential(CredentialMutationOutcome),
}

/// One op's failure, wrapping whichever domain error it produced.
#[derive(Debug, thiserror::Error)]
pub enum StateOpError {
    #[error(transparent)]
    Mount(#[from] MountWriteError),
    #[error(transparent)]
    Credential(#[from] CredentialWriteError),
}

/// `apply_batch`'s failure: either one op failed, identified by its index in
/// the submitted list, or the batch machinery itself failed outside any
/// single op (for example, advancing the mount revision).
#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error("batch op {index} failed: {error}")]
    Op { index: usize, error: StateOpError },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

impl Db<'_> {
    /// Apply every op in `ops`, in order, inside one transaction.
    pub(crate) async fn apply_batch(
        &mut self,
        mutation_id: MutationId,
        ops: Vec<StateOp>,
    ) -> Result<Vec<OpOutcome>, BatchError> {
        self.transact("state batch", async move |db| {
            db.apply_batch_ops(mutation_id, ops).await
        })
        .await
    }

    async fn apply_batch_ops(
        &mut self,
        mutation_id: MutationId,
        ops: Vec<StateOp>,
    ) -> Result<Vec<OpOutcome>, BatchError> {
        // Read (not reserve) the revision this batch would commit to a mount
        // row. Reading it unconditionally keeps every op's dispatch uniform;
        // only a batch that actually touches a mount persists it below.
        let touches_mounts = ops.iter().any(StateOp::touches_mounts);
        let revision = self.next_mount_revision().await?;
        let mut outcomes = Vec::with_capacity(ops.len());
        for (index, op) in ops.into_iter().enumerate() {
            let outcome = self
                .apply_state_op(op, revision, mutation_id)
                .await
                .map_err(|error| BatchError::Op { index, error })?;
            outcomes.push(outcome);
        }
        if touches_mounts {
            self.advance_mount_revision(revision).await?;
        }
        Ok(outcomes)
    }

    async fn apply_state_op(
        &mut self,
        op: StateOp,
        revision: MountRevision,
        mutation_id: MutationId,
    ) -> Result<OpOutcome, StateOpError> {
        match op {
            StateOp::CreateMount(document) => self
                .create_mount_row(document, revision, mutation_id)
                .await
                .map(OpOutcome::Mount)
                .map_err(StateOpError::Mount),
            StateOp::UpdateMount(document) => self
                .update_mount_row(document, revision, mutation_id)
                .await
                .map(OpOutcome::Mount)
                .map_err(StateOpError::Mount),
            StateOp::RemoveMount(name) => self
                .remove_mount_row(name, revision)
                .await
                .map(OpOutcome::Mount)
                .map_err(StateOpError::Mount),
            StateOp::SubmitCredential(document) => self
                .submit_credential_row(document, mutation_id)
                .await
                .map(OpOutcome::Credential)
                .map_err(StateOpError::Credential),
            StateOp::DeleteCredential(id) => self
                .delete_credential_row(id, mutation_id)
                .await
                .map(OpOutcome::Credential)
                .map_err(StateOpError::Credential),
            StateOp::RevokeCredential { id, scopes } => self
                .begin_credential_revocation_row(id, mutation_id, scopes)
                .await
                .map(OpOutcome::Credential)
                .map_err(StateOpError::Credential),
        }
    }
}
