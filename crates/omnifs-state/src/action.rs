//! Durable typed action acceptance and non-secret action receipts.

use crate::CredentialWriteError;
use crate::credential::CredentialDocument;
use crate::db::Db;
use crate::row::{RowExt as _, sql_int};
use anyhow::Context as _;
use omnifs_api::{ActionKind, ActionPhase, ActionReceipt};
use omnifs_auth::CredentialId;
use omnifs_core::{ActionId, ProviderId, ResourceDigest, ResourceKey, ResourceKind, ResourceName};
use sqlx::Row as _;
use sqlx::sqlite::{SqliteConnection, SqliteRow};

const ACTION_RECEIPT_LIMIT: i64 = 256;
const ACTION_INPUT_DOMAIN: &[u8] = b"omnifs-action-input-v1\0";

/// Secret-bearing input accepted only by the state writer.
pub struct CredentialActionRequest {
    pub action_id: ActionId,
    pub credential: ResourceName,
    pub expected_generation: u64,
    pub operation: CredentialActionOperation,
}

/// Non-secret request to restart one desired attachment runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentActionRequest {
    pub action_id: ActionId,
    pub attachment: ResourceName,
    pub base_action_generation: u64,
}

impl std::fmt::Debug for CredentialActionRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialActionRequest")
            .field("action_id", &self.action_id)
            .field("credential", &self.credential)
            .field("expected_generation", &self.expected_generation)
            .field("operation", &self.operation)
            .finish()
    }
}

pub enum CredentialActionOperation {
    SetMaterial(CredentialDocument),
    Revoke,
}

impl std::fmt::Debug for CredentialActionOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetMaterial(document) => formatter
                .debug_tuple("SetMaterial")
                .field(document)
                .finish(),
            Self::Revoke => formatter.write_str("Revoke"),
        }
    }
}

impl CredentialActionOperation {
    const fn kind(&self) -> ActionKind {
        match self {
            Self::SetMaterial(_) => ActionKind::SetCredentialMaterial,
            Self::Revoke => ActionKind::RevokeCredential,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ActionWriteError {
    #[error("action id {0} was already used for different input")]
    IdReuse(ActionId),
    #[error("credential resource `{0}` was not found")]
    ResourceNotFound(ResourceName),
    #[error("attachment resource `{0}` was not found")]
    AttachmentResourceNotFound(ResourceName),
    #[error("credential resource `{0}` has no material to act on")]
    ActionUnavailable(ResourceName),
    #[error("credential resource `{credential}` does not match submitted material: {detail}")]
    InvalidCredential {
        credential: ResourceName,
        detail: String,
    },
    #[error(
        "credential `{credential}` action generation changed; expected {expected}, found {actual}"
    )]
    GenerationConflict {
        credential: ResourceName,
        expected: u64,
        actual: u64,
    },
    #[error(
        "attachment `{attachment}` action generation changed; expected {expected}, found {actual}"
    )]
    AttachmentGenerationConflict {
        attachment: ResourceName,
        expected: u64,
        actual: u64,
    },
    #[error("credential `{target}` already has pending action {action_id}")]
    Busy {
        target: ResourceName,
        action_id: ActionId,
    },
    #[error("attachment `{target}` already has pending action {action_id}")]
    AttachmentBusy {
        target: ResourceName,
        action_id: ActionId,
    },
    #[error("action {0} was not found")]
    NotFound(ActionId),
    #[error("action {action_id} is terminal in phase {phase:?}")]
    Terminal {
        action_id: ActionId,
        phase: ActionPhase,
    },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

impl Db<'_> {
    pub(crate) async fn accept_credential_action(
        &mut self,
        request: CredentialActionRequest,
    ) -> Result<ActionReceipt, ActionWriteError> {
        let request_digest = action_request_digest(&request);
        self.transact("credential action acceptance", async move |db| {
            db.accept_credential_action_in_transaction(request, request_digest)
                .await
        })
        .await
    }

    async fn accept_credential_action_in_transaction(
        &mut self,
        request: CredentialActionRequest,
        request_digest: ResourceDigest,
    ) -> Result<ActionReceipt, ActionWriteError> {
        if let Some(receipt) =
            existing_action(self.raw(), request.action_id, request_digest).await?
        {
            return Ok(receipt);
        }
        let target = credential_action_target(self.raw(), &request.credential).await?;
        validate_action_operation(&request, &target)?;
        if let Some(action_id) =
            pending_action_for_target(self.raw(), ResourceKind::Credential, &request.credential)
                .await?
        {
            return Err(ActionWriteError::Busy {
                target: request.credential,
                action_id,
            });
        }
        let actual_generation = credential_action_generation(self.raw(), &target.id).await?;
        if actual_generation != request.expected_generation {
            return Err(ActionWriteError::GenerationConflict {
                credential: request.credential,
                expected: request.expected_generation,
                actual: actual_generation,
            });
        }
        let accepted_generation = actual_generation
            .checked_add(1)
            .context("credential action generation exhausted")?;
        let kind = request.operation.kind();
        match request.operation {
            CredentialActionOperation::SetMaterial(document) => {
                self.submit_credential_row(document)
                    .await
                    .map_err(|error| anyhow::anyhow!(error))?;
            },
            CredentialActionOperation::Revoke => {
                if let Err(error) = self
                    .begin_credential_revocation_row(
                        target.id.clone(),
                        request.action_id,
                        Vec::new(),
                    )
                    .await
                {
                    return match error {
                        CredentialWriteError::NotFound(_) => {
                            Err(ActionWriteError::ActionUnavailable(request.credential))
                        },
                        other => Err(ActionWriteError::Store(anyhow::anyhow!(other))),
                    };
                }
            },
        }
        sqlx::query(
            "UPDATE credentials SET action_generation = ?4, updated_at = unixepoch() \
             WHERE provider_name = ?1 AND scheme = ?2 AND account = ?3",
        )
        .bind(target.id.provider_name())
        .bind(target.id.scheme())
        .bind(target.id.account())
        .bind(sql_int(
            accepted_generation,
            "credential action generation",
        )?)
        .execute(self.raw())
        .await
        .context("advance credential action generation")?;
        let receipt = ActionReceipt {
            action_id: request.action_id,
            kind,
            target: ResourceKey::new(ResourceKind::Credential, request.credential),
            action_generation: accepted_generation,
            phase: ActionPhase::Accepted,
            error_code: None,
            detail: None,
        };
        insert_action(self.raw(), request_digest, &receipt).await?;
        prune_terminal_actions(self.raw()).await?;
        Ok(receipt)
    }

    pub(crate) async fn accept_attachment_action(
        &mut self,
        request: AttachmentActionRequest,
    ) -> Result<ActionReceipt, ActionWriteError> {
        let request_digest = attachment_action_digest(&request);
        self.transact("attachment action acceptance", async move |db| {
            db.accept_attachment_action_in_transaction(request, request_digest)
                .await
        })
        .await
    }

    async fn accept_attachment_action_in_transaction(
        &mut self,
        request: AttachmentActionRequest,
        request_digest: ResourceDigest,
    ) -> Result<ActionReceipt, ActionWriteError> {
        if let Some(receipt) =
            existing_action(self.raw(), request.action_id, request_digest).await?
        {
            return Ok(receipt);
        }
        attachment_action_target(self.raw(), &request.attachment).await?;
        if let Some(action_id) =
            pending_action_for_target(self.raw(), ResourceKind::Attachment, &request.attachment)
                .await?
        {
            return Err(ActionWriteError::AttachmentBusy {
                target: request.attachment,
                action_id,
            });
        }
        let actual_generation =
            attachment_action_generation(self.raw(), &request.attachment).await?;
        if actual_generation != request.base_action_generation {
            return Err(ActionWriteError::AttachmentGenerationConflict {
                attachment: request.attachment,
                expected: request.base_action_generation,
                actual: actual_generation,
            });
        }
        let accepted_generation = actual_generation
            .checked_add(1)
            .context("attachment action generation exhausted")?;
        persist_attachment_action_generation(self.raw(), &request.attachment, accepted_generation)
            .await?;
        let receipt = ActionReceipt {
            action_id: request.action_id,
            kind: ActionKind::RestartAttachment,
            target: ResourceKey::new(ResourceKind::Attachment, request.attachment),
            action_generation: accepted_generation,
            phase: ActionPhase::Accepted,
            error_code: None,
            detail: None,
        };
        insert_action(self.raw(), request_digest, &receipt).await?;
        prune_terminal_actions(self.raw()).await?;
        Ok(receipt)
    }

    pub(crate) async fn transition_action(
        &mut self,
        action_id: ActionId,
        phase: ActionPhase,
        error_code: Option<String>,
        detail: Option<String>,
    ) -> Result<ActionReceipt, ActionWriteError> {
        self.transact("action transition", async move |db| {
            let current = action_receipt(db.raw(), action_id)
                .await?
                .ok_or(ActionWriteError::NotFound(action_id))?;
            if is_terminal(current.phase) {
                return Err(ActionWriteError::Terminal {
                    action_id,
                    phase: current.phase,
                });
            }
            validate_transition_fields(phase, error_code.as_deref(), detail.as_deref())?;
            sqlx::query(
                "UPDATE action_receipts \
                 SET phase = ?2, error_code = ?3, detail = ?4, updated_at = unixepoch() \
                 WHERE action_id = ?1",
            )
            .bind(action_id.as_bytes().as_slice())
            .bind(action_phase_str(phase))
            .bind(error_code.as_deref())
            .bind(detail.as_deref())
            .execute(db.raw())
            .await
            .context("transition action receipt")?;
            Ok(ActionReceipt {
                phase,
                error_code,
                detail,
                ..current
            })
        })
        .await
    }
}

pub(crate) async fn action_receipt(
    connection: &mut SqliteConnection,
    action_id: ActionId,
) -> anyhow::Result<Option<ActionReceipt>> {
    sqlx::query(
        "SELECT action_id, kind, target_kind, target_name, action_generation, phase, \
                error_code, detail \
         FROM action_receipts WHERE action_id = ?1",
    )
    .bind(action_id.as_bytes().as_slice())
    .fetch_optional(connection)
    .await
    .context("read action receipt")?
    .as_ref()
    .map(decode_action_receipt)
    .transpose()
}

pub(crate) async fn pending_actions(
    connection: &mut SqliteConnection,
) -> anyhow::Result<Vec<ActionReceipt>> {
    sqlx::query(
        "SELECT action_id, kind, target_kind, target_name, action_generation, phase, \
                error_code, detail \
         FROM action_receipts \
         WHERE phase IN ('accepted', 'running', 'retrying') \
         ORDER BY created_at, action_id",
    )
    .fetch_all(connection)
    .await
    .context("read pending action receipts")?
    .iter()
    .map(decode_action_receipt)
    .collect()
}

pub(crate) async fn action_receipts(
    connection: &mut SqliteConnection,
) -> anyhow::Result<Vec<ActionReceipt>> {
    sqlx::query(
        "SELECT action_id, kind, target_kind, target_name, action_generation, phase, \
                error_code, detail \
         FROM action_receipts ORDER BY created_at, action_id",
    )
    .fetch_all(connection)
    .await
    .context("read action receipts")?
    .iter()
    .map(decode_action_receipt)
    .collect()
}

async fn existing_action(
    connection: &mut SqliteConnection,
    action_id: ActionId,
    request_digest: ResourceDigest,
) -> Result<Option<ActionReceipt>, ActionWriteError> {
    let row = sqlx::query(
        "SELECT action_id, kind, target_kind, target_name, action_generation, phase, \
                error_code, detail, request_digest \
         FROM action_receipts WHERE action_id = ?1",
    )
    .bind(action_id.as_bytes().as_slice())
    .fetch_optional(connection)
    .await
    .context("read existing action receipt")?;
    let Some(row) = row else {
        return Ok(None);
    };
    if ResourceDigest::from_bytes(row.digest("request_digest")?) != request_digest {
        return Err(ActionWriteError::IdReuse(action_id));
    }
    Ok(Some(decode_action_receipt(&row)?))
}

struct CredentialActionTarget {
    id: CredentialId,
    provider: ProviderId,
}

async fn credential_action_target(
    connection: &mut SqliteConnection,
    credential: &ResourceName,
) -> Result<CredentialActionTarget, ActionWriteError> {
    let row = sqlx::query(
        "SELECT providers.name AS provider_metadata_name, \
                provider_resources.provider_digest, \
                credential_resources.scheme, credential_resources.account \
         FROM credential_resources \
         JOIN provider_resources \
           ON provider_resources.name = credential_resources.provider_name \
         JOIN providers ON providers.digest = provider_resources.provider_digest \
         WHERE credential_resources.name = ?1",
    )
    .bind(credential.as_str())
    .fetch_optional(connection)
    .await
    .context("read credential action target")?
    .ok_or_else(|| ActionWriteError::ResourceNotFound(credential.clone()))?;
    Ok(CredentialActionTarget {
        id: CredentialId::new(
            row.text("provider_metadata_name")?,
            row.text("scheme")?,
            row.text("account")?,
        )
        .context("stored credential resource has invalid identity")?,
        provider: ProviderId::from_digest(row.digest("provider_digest")?),
    })
}

fn validate_action_operation(
    request: &CredentialActionRequest,
    target: &CredentialActionTarget,
) -> Result<(), ActionWriteError> {
    let CredentialActionOperation::SetMaterial(document) = &request.operation else {
        return Ok(());
    };
    let invalid = |detail: &str| ActionWriteError::InvalidCredential {
        credential: request.credential.clone(),
        detail: detail.to_owned(),
    };
    if document.id != target.id {
        return Err(invalid(
            "credential identity differs from the desired resource",
        ));
    }
    if document.provider != target.provider {
        return Err(invalid(
            "provider digest differs from the desired provider resource",
        ));
    }
    Ok(())
}

async fn credential_action_generation(
    connection: &mut SqliteConnection,
    id: &CredentialId,
) -> anyhow::Result<u64> {
    let generation = sqlx::query_scalar::<_, i64>(
        "SELECT action_generation FROM credentials \
         WHERE provider_name = ?1 AND scheme = ?2 AND account = ?3",
    )
    .bind(id.provider_name())
    .bind(id.scheme())
    .bind(id.account())
    .fetch_optional(connection)
    .await
    .context("read credential action generation")?;
    generation.map_or(Ok(0), |value| {
        u64::try_from(value).context("stored credential action generation is negative")
    })
}

async fn pending_action_for_target(
    connection: &mut SqliteConnection,
    kind: ResourceKind,
    target: &ResourceName,
) -> anyhow::Result<Option<ActionId>> {
    let value = sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT action_id FROM action_receipts \
         WHERE target_kind = ?1 AND target_name = ?2 \
           AND phase IN ('accepted', 'running', 'retrying')",
    )
    .bind(resource_kind_str(kind))
    .bind(target.as_str())
    .fetch_optional(connection)
    .await
    .context("read pending credential action")?;
    value
        .as_deref()
        .map(decode_action_id)
        .transpose()
        .context("decode pending credential action id")
}

async fn attachment_action_target(
    connection: &mut SqliteConnection,
    attachment: &ResourceName,
) -> Result<(), ActionWriteError> {
    sqlx::query_scalar::<_, i64>("SELECT 1 FROM attachment_resources WHERE name = ?1")
        .bind(attachment.as_str())
        .fetch_optional(connection)
        .await
        .context("read attachment action target")?
        .ok_or_else(|| ActionWriteError::AttachmentResourceNotFound(attachment.clone()))?;
    Ok(())
}

async fn attachment_action_generation(
    connection: &mut SqliteConnection,
    attachment: &ResourceName,
) -> anyhow::Result<u64> {
    let generation = sqlx::query_scalar::<_, i64>(
        "SELECT action_generation FROM attachment_instances WHERE name = ?1",
    )
    .bind(attachment.as_str())
    .fetch_optional(connection)
    .await
    .context("read attachment action generation")?;
    generation.map_or(Ok(0), |value| {
        u64::try_from(value).context("stored attachment action generation is negative")
    })
}

async fn persist_attachment_action_generation(
    connection: &mut SqliteConnection,
    attachment: &ResourceName,
    generation: u64,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO attachment_instances(\
             name, desired_version, observed_version, phase, runtime_instance, \
             action_generation, last_error_code, last_error_detail, retry_at, deleting, updated_at\
         ) VALUES (?1, NULL, NULL, 'pending', NULL, ?2, NULL, NULL, NULL, 0, unixepoch()) \
         ON CONFLICT(name) DO UPDATE SET \
             action_generation = excluded.action_generation, \
             updated_at = excluded.updated_at",
    )
    .bind(attachment.as_str())
    .bind(sql_int(generation, "attachment action generation")?)
    .execute(connection)
    .await
    .context("persist attachment action generation")?;
    Ok(())
}

fn attachment_action_digest(request: &AttachmentActionRequest) -> ResourceDigest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ACTION_INPUT_DOMAIN);
    hasher.update(&[action_kind_tag(ActionKind::RestartAttachment)]);
    hasher.update(&[ResourceKind::Attachment.tag()]);
    let target = request.attachment.as_str().as_bytes();
    hasher.update(
        u64::try_from(target.len())
            .expect("resource name length fits u64")
            .to_be_bytes()
            .as_slice(),
    );
    hasher.update(target);
    hasher.update(request.base_action_generation.to_be_bytes().as_slice());
    ResourceDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn action_request_digest(request: &CredentialActionRequest) -> ResourceDigest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ACTION_INPUT_DOMAIN);
    hasher.update(&[action_kind_tag(request.operation.kind())]);
    hasher.update(&[ResourceKind::Credential.tag()]);
    let target = request.credential.as_str().as_bytes();
    hasher.update(
        u64::try_from(target.len())
            .expect("resource name length fits u64")
            .to_be_bytes()
            .as_slice(),
    );
    hasher.update(target);
    hasher.update(request.expected_generation.to_be_bytes().as_slice());
    if let CredentialActionOperation::SetMaterial(document) = &request.operation {
        hasher.update(document.provider.as_bytes());
        hasher.update(&[match document.kind {
            omnifs_auth::AuthKind::StaticToken => 1,
            omnifs_auth::AuthKind::OAuth => 2,
        }]);
        hash_string(&mut hasher, document.id.provider_name());
        hash_string(&mut hasher, document.id.scheme());
        hash_string(&mut hasher, document.id.account());
        hasher.update(
            u64::try_from(document.scopes.len())
                .expect("scope count fits u64")
                .to_be_bytes()
                .as_slice(),
        );
        for scope in &document.scopes {
            hash_string(&mut hasher, scope);
        }
    }
    ResourceDigest::from_bytes(*hasher.finalize().as_bytes())
}

fn hash_string(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(
        u64::try_from(value.len())
            .expect("string length fits u64")
            .to_be_bytes()
            .as_slice(),
    );
    hasher.update(value.as_bytes());
}

async fn insert_action(
    connection: &mut SqliteConnection,
    request_digest: ResourceDigest,
    receipt: &ActionReceipt,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO action_receipts(\
             action_id, kind, target_kind, target_name, request_digest, \
             action_generation, phase, error_code, detail, created_at, updated_at\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, unixepoch(), unixepoch())",
    )
    .bind(receipt.action_id.as_bytes().as_slice())
    .bind(action_kind_str(receipt.kind))
    .bind(resource_kind_str(receipt.target.kind))
    .bind(receipt.target.name.as_str())
    .bind(request_digest.as_bytes().as_slice())
    .bind(sql_int(
        receipt.action_generation,
        "accepted action generation",
    )?)
    .bind(action_phase_str(receipt.phase))
    .execute(connection)
    .await
    .context("insert action receipt")?;
    Ok(())
}

async fn prune_terminal_actions(connection: &mut SqliteConnection) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM action_receipts \
         WHERE phase IN ('ready', 'failed') \
           AND rowid NOT IN (\
             SELECT rowid FROM action_receipts \
             WHERE phase IN ('ready', 'failed') \
             ORDER BY updated_at DESC, rowid DESC LIMIT ?1\
           )",
    )
    .bind(ACTION_RECEIPT_LIMIT)
    .execute(connection)
    .await
    .context("prune terminal action receipts")?;
    Ok(())
}

fn decode_action_receipt(row: &SqliteRow) -> anyhow::Result<ActionReceipt> {
    let action_id = decode_action_id(&row.bytes("action_id")?)?;
    let target_kind = parse_resource_kind(&row.text("target_kind")?)?;
    let action_generation: i64 = row
        .try_get("action_generation")
        .context("read accepted action generation")?;
    Ok(ActionReceipt {
        action_id,
        kind: parse_action_kind(&row.text("kind")?)?,
        target: ResourceKey::new(
            target_kind,
            ResourceName::new(row.text("target_name")?)
                .context("stored action target name is invalid")?,
        ),
        action_generation: u64::try_from(action_generation)
            .context("stored accepted action generation is negative")?,
        phase: parse_action_phase(&row.text("phase")?)?,
        error_code: row.optional_text("error_code")?,
        detail: row.optional_text("detail")?,
    })
}

fn decode_action_id(bytes: &[u8]) -> anyhow::Result<ActionId> {
    let length = bytes.len();
    let array: [u8; 16] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("stored action id has {length} bytes; expected 16"))?;
    Ok(ActionId::from_bytes(array))
}

const fn action_kind_tag(kind: ActionKind) -> u8 {
    match kind {
        ActionKind::SetCredentialMaterial => 1,
        ActionKind::RevokeCredential => 2,
        ActionKind::RestartAttachment => 3,
    }
}

const fn action_kind_str(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::SetCredentialMaterial => "set-credential-material",
        ActionKind::RevokeCredential => "revoke-credential",
        ActionKind::RestartAttachment => "restart-attachment",
    }
}

fn parse_action_kind(value: &str) -> anyhow::Result<ActionKind> {
    match value {
        "set-credential-material" => Ok(ActionKind::SetCredentialMaterial),
        "revoke-credential" => Ok(ActionKind::RevokeCredential),
        "restart-attachment" => Ok(ActionKind::RestartAttachment),
        _ => anyhow::bail!("stored action kind `{value}` is invalid"),
    }
}

const fn action_phase_str(phase: ActionPhase) -> &'static str {
    match phase {
        ActionPhase::Accepted => "accepted",
        ActionPhase::Running => "running",
        ActionPhase::Retrying => "retrying",
        ActionPhase::Ready => "ready",
        ActionPhase::Failed => "failed",
    }
}

fn parse_action_phase(value: &str) -> anyhow::Result<ActionPhase> {
    match value {
        "accepted" => Ok(ActionPhase::Accepted),
        "running" => Ok(ActionPhase::Running),
        "retrying" => Ok(ActionPhase::Retrying),
        "ready" => Ok(ActionPhase::Ready),
        "failed" => Ok(ActionPhase::Failed),
        _ => anyhow::bail!("stored action phase `{value}` is invalid"),
    }
}

const fn is_terminal(phase: ActionPhase) -> bool {
    matches!(phase, ActionPhase::Ready | ActionPhase::Failed)
}

fn validate_transition_fields(
    phase: ActionPhase,
    error_code: Option<&str>,
    detail: Option<&str>,
) -> Result<(), ActionWriteError> {
    let failed = matches!(phase, ActionPhase::Failed);
    if failed != error_code.is_some() || failed != detail.is_some() {
        return Err(anyhow::anyhow!(
            "failed actions require an error code and detail; other phases forbid both"
        )
        .into());
    }
    if matches!(phase, ActionPhase::Accepted) {
        return Err(
            anyhow::anyhow!("an accepted action cannot transition back to accepted").into(),
        );
    }
    Ok(())
}

const fn resource_kind_str(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Credential => "credential",
        ResourceKind::Attachment => "attachment",
        ResourceKind::Provider => "provider",
        ResourceKind::Mount => "mount",
    }
}

fn parse_resource_kind(value: &str) -> anyhow::Result<ResourceKind> {
    match value {
        "credential" => Ok(ResourceKind::Credential),
        "attachment" => Ok(ResourceKind::Attachment),
        _ => anyhow::bail!("stored action target kind `{value}` is invalid"),
    }
}
