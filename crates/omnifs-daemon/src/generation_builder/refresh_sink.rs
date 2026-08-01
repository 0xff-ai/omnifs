//! Persists a routine or authority-changed OAuth refresh back into durable
//! state. This is the daemon's sole implementation of `omnifs_auth::RefreshSink`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use omnifs_auth::{
    AuthKind, CredentialId, RefreshCandidate, RefreshClassification, RefreshPersistError,
    RefreshPersistence, RefreshSink,
};
use omnifs_state::{
    CredentialDocument, CredentialRefreshKind, CredentialState, CredentialWriteError,
    SecretMaterial, StateStore,
};

use super::CredentialRuntime;
use crate::credential_codec::encode_refreshed_payload;

pub(super) struct StateRefreshSink {
    state: Arc<StateStore>,
    credentials: Arc<HashMap<CredentialId, CredentialRuntime>>,
}

impl StateRefreshSink {
    pub(super) fn new(
        state: Arc<StateStore>,
        credentials: Arc<HashMap<CredentialId, CredentialRuntime>>,
    ) -> Self {
        Self { state, credentials }
    }
}

impl RefreshSink for StateRefreshSink {
    fn persist<'a>(
        &'a self,
        candidate: RefreshCandidate,
    ) -> Pin<Box<dyn Future<Output = Result<RefreshPersistence, RefreshPersistError>> + Send + 'a>>
    {
        Box::pin(async move {
            let credential = self
                .credentials
                .get(&candidate.credential_id)
                .ok_or(RefreshPersistError::Rejected)?;
            if credential.kind != AuthKind::OAuth {
                return Err(RefreshPersistError::Rejected);
            }
            let material = encode_refreshed_payload(&candidate.refreshed, &credential.overrides)
                .map_err(|_| RefreshPersistError::Rejected)?;
            let document = CredentialDocument {
                id: candidate.credential_id,
                provider: credential.provider,
                kind: credential.kind,
                auth_fingerprint: credential.fingerprint,
                scopes: candidate.refreshed.scopes().to_vec(),
                material: SecretMaterial::new(material),
            };
            let kind = match candidate.classification {
                RefreshClassification::Routine => CredentialRefreshKind::Routine,
                RefreshClassification::AuthorityChanged => CredentialRefreshKind::AuthorityChanged,
            };
            let outcome = self
                .state
                .refresh_credential(document, candidate.expected_version, kind)
                .await
                .map_err(|error| map_refresh_error(&error))?;
            match outcome.state {
                CredentialState::Active => Ok(RefreshPersistence::Active {
                    version: outcome.version,
                }),
                CredentialState::PendingRepublish => Ok(RefreshPersistence::PendingRepublish {
                    version: outcome.version,
                }),
                _ => Err(RefreshPersistError::Rejected),
            }
        })
    }
}

fn map_refresh_error(error: &CredentialWriteError) -> RefreshPersistError {
    match error {
        CredentialWriteError::Conflict { .. } => RefreshPersistError::Conflict,
        CredentialWriteError::Store(_) => RefreshPersistError::Unavailable,
        CredentialWriteError::NotFound(_)
        | CredentialWriteError::GenerationConflict { .. }
        | CredentialWriteError::FactsMismatch { .. }
        | CredentialWriteError::InvalidState { .. } => RefreshPersistError::Rejected,
    }
}
