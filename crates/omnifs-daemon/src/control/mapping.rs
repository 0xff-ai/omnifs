//! Projection from durable state types onto the `omnifs-api` control
//! vocabulary the CLI and Inspector consume.

use super::*;

pub(crate) fn api_mount_record(
    mount: omnifs_state::StoredMount,
    health: MountHealth,
    auth_health: Option<CredentialHealth>,
) -> anyhow::Result<MountRecord> {
    let last_mutation_id = mount.last_mutation_id;
    let document = mount.document;
    let provider = api_provider_reference(document.provider.clone());
    let definition = ApiMountDefinition {
        name: document.name,
        provider: document.provider.id,
        auth: document.credential.map(|id| omnifs_api::MountCredential {
            scheme: id.scheme().to_owned(),
            account_label: id.account().to_owned(),
        }),
        limits: document.limits.map(|limits| ApiMountLimits {
            max_memory_mb: limits.max_memory_mb,
            max_fetch_blob_bytes: limits.max_fetch_blob_bytes,
        }),
        config: serde_json::to_vec(&document.config).context("encode mount config")?,
    };
    Ok(MountRecord {
        definition,
        provider,
        version: mount.version,
        revision: mount.revision,
        health,
        auth_health,
        last_mutation_id,
    })
}

pub(crate) fn credential_id(
    key: CredentialKey,
) -> Result<omnifs_auth::CredentialId, omnifs_auth::CredentialIdError> {
    omnifs_auth::CredentialId::new(key.provider_name, key.scheme, key.account_label)
}

pub(crate) async fn api_credential_status(
    state: &StateStore,
    summary: omnifs_state::CredentialSummary,
) -> anyhow::Result<CredentialStatus> {
    let scopes = state
        .get_credential(&summary.id)
        .await?
        .map(|stored| credential_scopes(&stored))
        .transpose()?
        .unwrap_or_default();
    Ok(api_credential_status_with_scopes(&summary, scopes))
}

pub(crate) fn api_credential_status_with_scopes(
    summary: &omnifs_state::CredentialSummary,
    scopes: Vec<String>,
) -> CredentialStatus {
    CredentialStatus {
        key: CredentialKey {
            provider_name: summary.id.provider_name().to_owned(),
            scheme: summary.id.scheme().to_owned(),
            account_label: summary.id.account().to_owned(),
        },
        provider: summary.provider,
        kind: api_credential_kind(summary.kind),
        scopes,
        auth_fingerprint: summary.auth_fingerprint,
        version: summary.version,
        generation: summary.generation,
        action_generation: summary.action_generation,
        status: api_credential_status_kind(summary.state),
        last_mutation_id: summary.last_mutation_id,
    }
}

pub(crate) fn api_provider_metadata(
    provider: omnifs_state::StoredProviderMetadata,
) -> ProviderMetadata {
    ProviderMetadata {
        reference: api_provider_reference(provider.reference),
        manifest: provider.document,
    }
}

pub(crate) fn api_provider_reference(reference: omnifs_core::ProviderRef) -> ProviderReference {
    ProviderReference {
        id: reference.id,
        name: reference.meta.name.to_string(),
        version: reference.meta.version.map(|version| version.to_string()),
    }
}

pub(crate) const fn api_provider_import_disposition(
    disposition: omnifs_state::ProviderImportDisposition,
) -> ProviderImportDisposition {
    match disposition {
        omnifs_state::ProviderImportDisposition::Inserted => ProviderImportDisposition::Inserted,
        omnifs_state::ProviderImportDisposition::Unchanged => ProviderImportDisposition::Unchanged,
        omnifs_state::ProviderImportDisposition::Repaired => ProviderImportDisposition::Repaired,
    }
}

/// Project one batch op's durable outcome onto the wire result the client
/// sees, in submitted order.
pub(crate) fn api_mutation_op_result(outcome: &omnifs_state::OpOutcome) -> MutationOpResult {
    match outcome {
        omnifs_state::OpOutcome::Mount(outcome) => MutationOpResult::Mount(MountOpResult {
            name: outcome.name.clone(),
            version: outcome.version,
            revision: outcome.revision,
        }),
        omnifs_state::OpOutcome::Credential(outcome) => {
            MutationOpResult::Credential(api_credential_outcome(outcome.clone()))
        },
    }
}

fn api_credential_outcome(outcome: omnifs_state::CredentialMutationOutcome) -> CredentialStatus {
    CredentialStatus {
        key: CredentialKey {
            provider_name: outcome.provider_name,
            scheme: outcome.scheme,
            account_label: outcome.account_label,
        },
        provider: outcome.provider,
        kind: api_credential_kind(outcome.kind),
        scopes: outcome.scopes,
        auth_fingerprint: outcome.auth_fingerprint,
        version: outcome.version,
        generation: outcome.generation,
        action_generation: 0,
        status: api_credential_status_kind(outcome.state),
        last_mutation_id: outcome.last_mutation_id,
    }
}

const fn api_credential_kind(kind: omnifs_auth::AuthKind) -> CredentialKind {
    match kind {
        omnifs_auth::AuthKind::StaticToken => CredentialKind::StaticToken,
        omnifs_auth::AuthKind::OAuth => CredentialKind::OAuth,
    }
}

const fn api_credential_status_kind(state: omnifs_state::CredentialState) -> CredentialStatusKind {
    match state {
        omnifs_state::CredentialState::Active => CredentialStatusKind::Active,
        omnifs_state::CredentialState::Blocked => CredentialStatusKind::Blocked,
        omnifs_state::CredentialState::PendingRepublish => CredentialStatusKind::PendingRepublish,
        omnifs_state::CredentialState::RevocationPending => CredentialStatusKind::RevocationPending,
        omnifs_state::CredentialState::RevocationUnknown => CredentialStatusKind::RevocationUnknown,
        omnifs_state::CredentialState::Deleted => CredentialStatusKind::Deleted,
    }
}

pub(crate) fn manager_error(error: &ManagerError) -> ControlError {
    let (code, message) = match error {
        ManagerError::Busy => (ControlErrorCode::Busy, error.to_string()),
        ManagerError::Stopped => (ControlErrorCode::NotReady, error.to_string()),
        ManagerError::MutationInProgress { .. } => {
            (ControlErrorCode::MutationInProgress, error.to_string())
        },
        ManagerError::LeaseExpired(_) => (ControlErrorCode::LeaseExpired, error.to_string()),
        ManagerError::LeaseNotHeld(_) => (ControlErrorCode::LeaseNotHeld, error.to_string()),
        ManagerError::RecoveryRequired(_) => {
            (ControlErrorCode::RecoveryRequired, error.to_string())
        },
        ManagerError::Invalid(_) | ManagerError::CredentialId(_) => {
            (ControlErrorCode::InvalidRequest, error.to_string())
        },
        ManagerError::Mount(inner) => (mount_write_error_code(inner), error.to_string()),
        ManagerError::Credential(inner) => (credential_write_error_code(inner), error.to_string()),
        ManagerError::Batch(inner) => return batch_error(inner),
        ManagerError::Other(_) | ManagerError::Task(_) => {
            (ControlErrorCode::Internal, error.to_string())
        },
    };
    ControlError::new(code, message)
}

pub(crate) fn resource_control_error(
    error: &crate::resource_control::ResourceControlError,
) -> ControlError {
    use crate::resource_control::ResourceControlError;

    let code = match error {
        ResourceControlError::ShuttingDown => ControlErrorCode::NotReady,
        ResourceControlError::PlanTooLarge { .. } => ControlErrorCode::PlanTooLarge,
        ResourceControlError::MissingProviderArtifact(_) => {
            ControlErrorCode::MissingProviderArtifact
        },
        ResourceControlError::InvalidCredentialScheme { .. }
        | ResourceControlError::CredentialNotFound(_)
        | ResourceControlError::DuplicateCredentialMaterial(_)
        | ResourceControlError::InvalidCredentialMaterial(_)
        | ResourceControlError::Definition(
            omnifs_api::ResourceDefinitionError::InvalidCredentialField(_)
            | omnifs_api::ResourceDefinitionError::MountConfigNotObject(_)
            | omnifs_api::ResourceDefinitionError::MissingProvider { .. }
            | omnifs_api::ResourceDefinitionError::MissingCredentialProvider { .. }
            | omnifs_api::ResourceDefinitionError::MissingCredential { .. }
            | omnifs_api::ResourceDefinitionError::CredentialProviderMismatch { .. }
            | omnifs_api::ResourceDefinitionError::DuplicateKey(_),
        )
        | ResourceControlError::Apply(
            omnifs_state::ResourceApplyError::InvalidCredentialSecret { .. },
        )
        | ResourceControlError::Action(omnifs_state::ActionWriteError::InvalidCredential {
            ..
        }) => ControlErrorCode::InvalidResource,
        ResourceControlError::Definition(
            omnifs_api::ResourceDefinitionError::UnsupportedApiVersion(_),
        ) => ControlErrorCode::UnsupportedApiVersion,
        ResourceControlError::DesiredDigestMismatch
        | ResourceControlError::Apply(omnifs_state::ResourceApplyError::DesiredDigestMismatch) => {
            ControlErrorCode::DesiredDigestMismatch
        },
        ResourceControlError::Apply(omnifs_state::ResourceApplyError::MutationIdReuse(_)) => {
            ControlErrorCode::MutationIdReuseMismatch
        },
        ResourceControlError::Apply(omnifs_state::ResourceApplyError::StaleRevision { .. }) => {
            ControlErrorCode::StaleBaseRevision
        },
        ResourceControlError::Action(omnifs_state::ActionWriteError::IdReuse(_)) => {
            ControlErrorCode::ActionIdReuseMismatch
        },
        ResourceControlError::Action(
            omnifs_state::ActionWriteError::ResourceNotFound(_)
            | omnifs_state::ActionWriteError::AttachmentResourceNotFound(_)
            | omnifs_state::ActionWriteError::ActionUnavailable(_)
            | omnifs_state::ActionWriteError::NotFound(_),
        ) => ControlErrorCode::ActionUnavailable,
        ResourceControlError::Action(
            omnifs_state::ActionWriteError::GenerationConflict { .. }
            | omnifs_state::ActionWriteError::AttachmentGenerationConflict { .. }
            | omnifs_state::ActionWriteError::Terminal { .. },
        ) => ControlErrorCode::Conflict,
        ResourceControlError::Action(
            omnifs_state::ActionWriteError::Busy { .. }
            | omnifs_state::ActionWriteError::AttachmentBusy { .. },
        ) => ControlErrorCode::Busy,
        ResourceControlError::Apply(omnifs_state::ResourceApplyError::Store(_))
        | ResourceControlError::Action(omnifs_state::ActionWriteError::Store(_))
        | ResourceControlError::Other(_) => ControlErrorCode::Internal,
    };
    ControlError::new(code, error.to_string())
}

const fn mount_write_error_code(error: &omnifs_state::MountWriteError) -> ControlErrorCode {
    match error {
        omnifs_state::MountWriteError::AlreadyExists(_) => ControlErrorCode::AlreadyExists,
        omnifs_state::MountWriteError::NotFound(_) => ControlErrorCode::NotFound,
        omnifs_state::MountWriteError::Store(_) => ControlErrorCode::Internal,
    }
}

const fn credential_write_error_code(
    error: &omnifs_state::CredentialWriteError,
) -> ControlErrorCode {
    match error {
        omnifs_state::CredentialWriteError::NotFound(_) => ControlErrorCode::NotFound,
        omnifs_state::CredentialWriteError::Conflict { .. }
        | omnifs_state::CredentialWriteError::GenerationConflict { .. } => {
            ControlErrorCode::Conflict
        },
        omnifs_state::CredentialWriteError::FactsMismatch { .. }
        | omnifs_state::CredentialWriteError::InvalidState { .. } => {
            ControlErrorCode::InvalidRequest
        },
        omnifs_state::CredentialWriteError::Store(_) => ControlErrorCode::Internal,
    }
}

/// Map a failed batch op onto the same error vocabulary a standalone mount or
/// credential failure would use, with the failing op's index folded into the
/// message since the wire error envelope carries no structured detail channel.
fn batch_error(error: &omnifs_state::BatchError) -> ControlError {
    match error {
        omnifs_state::BatchError::Op {
            index,
            error: op_error,
        } => {
            let code = match op_error {
                omnifs_state::StateOpError::Mount(inner) => mount_write_error_code(inner),
                omnifs_state::StateOpError::Credential(inner) => credential_write_error_code(inner),
            };
            ControlError::new(code, format!("batch op {index} failed: {op_error}"))
        },
        omnifs_state::BatchError::Store(_) => {
            ControlError::new(ControlErrorCode::Internal, error.to_string())
        },
    }
}

pub(crate) fn api_credential_health_kind(
    health: &omnifs_auth::CredentialHealth,
) -> CredentialHealth {
    match health {
        omnifs_auth::CredentialHealth::Ready => CredentialHealth::Ready,
        omnifs_auth::CredentialHealth::ExpiringSoon => CredentialHealth::ExpiringSoon,
        omnifs_auth::CredentialHealth::Expired => CredentialHealth::Expired,
        omnifs_auth::CredentialHealth::RefreshFailed { .. } => CredentialHealth::RefreshFailed,
        omnifs_auth::CredentialHealth::NeedsConsent => CredentialHealth::NeedsConsent,
        omnifs_auth::CredentialHealth::Missing => CredentialHealth::Missing,
        omnifs_auth::CredentialHealth::StaticUnvalidated => CredentialHealth::StaticUnvalidated,
    }
}
