//! Durable desired resources, one-time legacy backfill, and atomic full-set apply.

pub(crate) mod codec;

use crate::credential::{CredentialDocument, CredentialSummary, credential_summaries_query};
use crate::db::Db;
use crate::mount::{MountLimits, StoredMount, mounts_query};
use crate::row::{RowExt as _, sql_int};
use anyhow::Context as _;
use omnifs_api::{
    ApplyReceipt, AttachmentDefinition, CredentialDefinition, MountResourceDefinition,
    NormalizedResourceSet, ProviderDefinition, ResourceChangeAction, ResourceDefinition,
    ResourceLimits, plan,
};
use omnifs_core::{
    AttachmentVersion, MutationId, ProviderId, ResourceDigest, ResourceKind, ResourceName,
    ResourceRevision,
};
use sqlx::sqlite::{SqliteConnection, SqliteRow};
use sqlx::{Row as _, SqlitePool};
use std::collections::{BTreeMap, BTreeSet};

use codec::{decode_attachment, decode_mount, encode_attachment, encode_mount};

const APPLY_RECEIPT_LIMIT: i64 = 256;
const APPLY_INPUT_DOMAIN: &[u8] = b"omnifs-resource-apply-input-v1\0";

/// One transactionally consistent non-secret desired-state head.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceSnapshot {
    pub revision: ResourceRevision,
    pub desired_digest: ResourceDigest,
    pub resources: NormalizedResourceSet,
}

/// One exact desired attachment row with its durable content version and
/// resource revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredAttachment {
    pub definition: omnifs_api::AttachmentDefinition,
    pub version: AttachmentVersion,
    pub revision: ResourceRevision,
}

/// Request-only credential material paired with one credential resource.
pub struct CredentialSecretSidecar {
    pub credential: ResourceName,
    pub document: CredentialDocument,
}

impl std::fmt::Debug for CredentialSecretSidecar {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialSecretSidecar")
            .field("credential", &self.credential)
            .field("document", &self.document)
            .finish()
    }
}

/// One complete desired-set compare-and-swap request.
#[derive(Debug)]
pub struct ResourceApplyRequest {
    pub mutation_id: MutationId,
    pub base_revision: ResourceRevision,
    pub expected_desired_digest: ResourceDigest,
    pub desired: NormalizedResourceSet,
    pub credential_secrets: Vec<CredentialSecretSidecar>,
}

#[derive(Debug, thiserror::Error)]
pub enum ResourceApplyError {
    #[error("desired resource digest does not match the normalized declarations")]
    DesiredDigestMismatch,
    #[error("mutation id {0} was already used for different input")]
    MutationIdReuse(MutationId),
    #[error("desired resources changed; expected revision {expected:?}, found {actual:?}")]
    StaleRevision {
        expected: ResourceRevision,
        actual: ResourceRevision,
    },
    #[error("invalid credential secret sidecar for {credential}: {detail}")]
    InvalidCredentialSecret {
        credential: ResourceName,
        detail: String,
    },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

impl Db<'_> {
    pub(crate) async fn initialize_resources(&mut self) -> anyhow::Result<()> {
        self.transact("resource backfill", async |db| {
            if resource_initialized(db.raw()).await? {
                return Ok(());
            }
            let definitions = legacy_resource_definitions(db.raw()).await?;
            let normalized =
                NormalizedResourceSet::new(definitions).context("normalize migrated resources")?;
            let mount_revision: i64 =
                sqlx::query_scalar("SELECT revision FROM mount_state WHERE singleton = 1")
                    .fetch_one(db.raw())
                    .await
                    .context("read legacy mount revision")?;
            let revision = ResourceRevision::new(
                u64::try_from(mount_revision)
                    .context("legacy mount revision is negative")?
                    .max(1),
            );
            let mutation_id = MutationId::from_bytes([0; 16]);
            for resource in normalized.resources() {
                write_resource(db.raw(), resource, revision, mutation_id).await?;
            }
            sqlx::query(
                "UPDATE resource_state \
                 SET revision = ?1, desired_digest = ?2, initialized = 1, \
                     updated_at = unixepoch() \
                 WHERE singleton = 1",
            )
            .bind(sql_int(revision.get(), "resource revision")?)
            .bind(normalized.digest().as_bytes().as_slice())
            .execute(db.raw())
            .await
            .context("finish resource backfill")?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn apply_resources(
        &mut self,
        request: ResourceApplyRequest,
    ) -> Result<ApplyReceipt, ResourceApplyError> {
        if request.expected_desired_digest != request.desired.digest() {
            return Err(ResourceApplyError::DesiredDigestMismatch);
        }
        validate_secret_sidecars(&request.desired, &request.credential_secrets)?;
        let input_digest = apply_input_digest(&request);
        self.transact("resource apply", async move |db| {
            db.apply_resources_in_transaction(request, input_digest)
                .await
        })
        .await
    }

    /// Keep the temporary imperative mutation surface restart-safe until its
    /// Plan 009 removal. Once any declarative apply receipt exists, that API
    /// owns desired state and legacy batches must not rewrite it.
    pub(crate) async fn sync_legacy_resources_if_unclaimed(
        &mut self,
        mutation_id: MutationId,
    ) -> anyhow::Result<()> {
        let receipt_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apply_receipts")
            .fetch_one(self.raw())
            .await
            .context("count declarative resource receipts")?;
        if receipt_count != 0 {
            return Ok(());
        }

        let desired = NormalizedResourceSet::new(legacy_resource_definitions(self.raw()).await?)
            .context("normalize legacy resource mirror")?;
        let current = read_resource_snapshot(self.raw()).await?;
        if current.desired_digest == desired.digest() {
            return Ok(());
        }
        let revision = current
            .revision
            .next()
            .context("legacy resource mirror revision exhausted")?;
        let changes = plan(&current.resources, &desired);
        apply_resource_row_changes(self.raw(), &changes, &desired, revision, mutation_id).await?;
        sqlx::query(
            "UPDATE resource_state \
             SET revision = ?1, desired_digest = ?2, updated_at = unixepoch() \
             WHERE singleton = 1 AND initialized = 1",
        )
        .bind(sql_int(revision.get(), "resource revision")?)
        .bind(desired.digest().as_bytes().as_slice())
        .execute(self.raw())
        .await
        .context("advance mirrored legacy resource state")?;
        Ok(())
    }

    async fn apply_resources_in_transaction(
        &mut self,
        request: ResourceApplyRequest,
        input_digest: ResourceDigest,
    ) -> Result<ApplyReceipt, ResourceApplyError> {
        if let Some(receipt) =
            existing_receipt(self.raw(), request.mutation_id, input_digest).await?
        {
            return Ok(receipt);
        }

        let current = read_resource_snapshot(self.raw()).await?;
        if current.desired_digest == request.desired.digest() {
            let receipt = ApplyReceipt {
                mutation_id: request.mutation_id,
                revision: current.revision,
                desired_digest: current.desired_digest,
                created: 0,
                updated: 0,
                deleted: 0,
                changed: false,
            };
            write_receipt(self.raw(), input_digest, &receipt).await?;
            return Ok(receipt);
        }
        if current.revision != request.base_revision {
            return Err(ResourceApplyError::StaleRevision {
                expected: request.base_revision,
                actual: current.revision,
            });
        }

        let changes = plan(&current.resources, &request.desired);
        let created = count_changes(&changes, ResourceChangeAction::Create)?;
        let updated = count_changes(&changes, ResourceChangeAction::Update)?;
        let deleted = count_changes(&changes, ResourceChangeAction::Delete)?;
        let revision = current
            .revision
            .next()
            .context("resource revision exhausted")?;

        apply_resource_row_changes(
            self.raw(),
            &changes,
            &request.desired,
            revision,
            request.mutation_id,
        )
        .await?;
        for sidecar in request.credential_secrets {
            self.submit_credential_row(sidecar.document, request.mutation_id)
                .await
                .map_err(|error| anyhow::anyhow!(error))?;
        }

        sqlx::query(
            "UPDATE resource_state \
             SET revision = ?1, desired_digest = ?2, updated_at = unixepoch() \
             WHERE singleton = 1 AND initialized = 1",
        )
        .bind(sql_int(revision.get(), "resource revision")?)
        .bind(request.desired.digest().as_bytes().as_slice())
        .execute(self.raw())
        .await
        .context("advance desired resource state")?;

        let receipt = ApplyReceipt {
            mutation_id: request.mutation_id,
            revision,
            desired_digest: request.desired.digest(),
            created,
            updated,
            deleted,
            changed: true,
        };
        write_receipt(self.raw(), input_digest, &receipt).await?;
        Ok(receipt)
    }
}

pub(crate) async fn snapshot(pool: &SqlitePool) -> anyhow::Result<ResourceSnapshot> {
    let mut transaction = pool.begin().await.context("begin resource snapshot")?;
    let snapshot = read_resource_snapshot(&mut transaction).await?;
    transaction
        .commit()
        .await
        .context("release resource snapshot")?;
    Ok(snapshot)
}

async fn read_resource_snapshot(
    connection: &mut SqliteConnection,
) -> anyhow::Result<ResourceSnapshot> {
    let (revision, desired_digest) = read_resource_head(connection).await?;
    let mut resources = Vec::new();
    read_provider_resources(connection, &mut resources).await?;
    read_credential_resources(connection, &mut resources).await?;
    read_mount_resources(connection, &mut resources).await?;
    read_attachment_resources(connection, &mut resources).await?;
    let resources =
        NormalizedResourceSet::new(resources).context("validate stored desired resources")?;
    anyhow::ensure!(
        resources.digest() == desired_digest,
        "stored desired resource digest does not match resource rows"
    );
    Ok(ResourceSnapshot {
        revision,
        desired_digest,
        resources,
    })
}

async fn read_resource_head(
    connection: &mut SqliteConnection,
) -> anyhow::Result<(ResourceRevision, ResourceDigest)> {
    let (revision, digest, initialized) = sqlx::query_as::<_, (i64, Vec<u8>, i64)>(
        "SELECT revision, desired_digest, initialized \
             FROM resource_state WHERE singleton = 1",
    )
    .fetch_one(&mut *connection)
    .await
    .context("read resource state")?;
    anyhow::ensure!(initialized == 1, "resource state is not initialized");
    let revision =
        ResourceRevision::new(u64::try_from(revision).context("resource revision is negative")?);
    let desired_digest =
        ResourceDigest::from_bytes(digest.try_into().map_err(|bytes: Vec<u8>| {
            anyhow::anyhow!(
                "stored resource digest has {} bytes; expected 32",
                bytes.len()
            )
        })?);
    Ok((revision, desired_digest))
}

async fn read_provider_resources(
    connection: &mut SqliteConnection,
    resources: &mut Vec<ResourceDefinition>,
) -> anyhow::Result<()> {
    for row in sqlx::query("SELECT name, provider_digest FROM provider_resources ORDER BY name")
        .fetch_all(&mut *connection)
        .await
        .context("read provider resources")?
    {
        let name_text: String = row.try_get("name").context("read provider resource name")?;
        let name = ResourceName::new(name_text.clone())
            .with_context(|| format!("decode provider resource `{name_text}`"))?;
        resources.push(ResourceDefinition::Provider(ProviderDefinition {
            name,
            artifact: ProviderId::from_digest(
                row.digest("provider_digest")
                    .with_context(|| format!("decode provider resource `{name_text}`"))?,
            ),
        }));
    }
    Ok(())
}

async fn read_credential_resources(
    connection: &mut SqliteConnection,
    resources: &mut Vec<ResourceDefinition>,
) -> anyhow::Result<()> {
    for row in sqlx::query(
        "SELECT name, provider_name, scheme, account \
         FROM credential_resources ORDER BY name",
    )
    .fetch_all(&mut *connection)
    .await
    .context("read credential resources")?
    {
        let name_text: String = row
            .try_get("name")
            .context("read credential resource name")?;
        let definition = CredentialDefinition {
            name: ResourceName::new(name_text.clone())
                .with_context(|| format!("decode credential resource `{name_text}`"))?,
            provider: ResourceName::new(row.text("provider_name")?)
                .with_context(|| format!("decode credential resource `{name_text}` provider"))?,
            scheme: row.text("scheme")?,
            account: row.text("account")?,
        };
        resources.push(ResourceDefinition::Credential(definition));
    }
    Ok(())
}

async fn read_mount_resources(
    connection: &mut SqliteConnection,
    resources: &mut Vec<ResourceDefinition>,
) -> anyhow::Result<()> {
    for row in sqlx::query(
        "SELECT name, canonical, version, provider_name, credential_name \
         FROM mount_resources ORDER BY name",
    )
    .fetch_all(&mut *connection)
    .await
    .context("read mount resources")?
    {
        let name_text: String = row.try_get("name").context("read mount resource name")?;
        let canonical = row.bytes("canonical")?;
        let definition = decode_mount(&canonical, row.digest("version")?)
            .with_context(|| format!("decode mount resource `{name_text}`"))?;
        anyhow::ensure!(
            definition.name.as_str() == name_text
                && definition.provider.as_str() == row.text("provider_name")?
                && definition.credential.as_ref().map(ResourceName::as_str)
                    == row.optional_text("credential_name")?.as_deref(),
            "mount resource `{name_text}` indexed fields do not match canonical bytes"
        );
        resources.push(ResourceDefinition::Mount(definition));
    }
    Ok(())
}

async fn read_attachment_resources(
    connection: &mut SqliteConnection,
    resources: &mut Vec<ResourceDefinition>,
) -> anyhow::Result<()> {
    for row in
        sqlx::query("SELECT name, canonical, version FROM attachment_resources ORDER BY name")
            .fetch_all(&mut *connection)
            .await
            .context("read attachment resources")?
    {
        let name_text: String = row
            .try_get("name")
            .context("read attachment resource name")?;
        let canonical = row.bytes("canonical")?;
        let definition = decode_attachment(
            &canonical,
            AttachmentVersion::from_digest(row.digest("version")?),
        )
        .with_context(|| format!("decode attachment resource `{name_text}`"))?;
        anyhow::ensure!(
            definition.name.as_str() == name_text,
            "attachment resource `{name_text}` name does not match canonical bytes"
        );
        resources.push(ResourceDefinition::Attachment(definition));
    }
    Ok(())
}

pub(crate) async fn desired_attachments(
    pool: &SqlitePool,
) -> anyhow::Result<Vec<DesiredAttachment>> {
    let mut attachments = Vec::new();
    for row in sqlx::query(
        "SELECT name, canonical, version, revision \
         FROM attachment_resources ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .context("read desired attachment resources")?
    {
        let name_text: String = row
            .try_get("name")
            .context("read desired attachment resource name")?;
        let version = AttachmentVersion::from_digest(row.digest("version")?);
        let definition = decode_attachment(&row.bytes("canonical")?, version)
            .with_context(|| format!("decode desired attachment resource `{name_text}`"))?;
        anyhow::ensure!(
            definition.name.as_str() == name_text,
            "attachment resource `{name_text}` name does not match canonical bytes"
        );
        attachments.push(DesiredAttachment {
            definition,
            version,
            revision: ResourceRevision::new(row.unsigned("revision")?),
        });
    }
    Ok(attachments)
}

async fn apply_resource_row_changes(
    connection: &mut SqliteConnection,
    changes: &[omnifs_api::ResourceChange],
    desired: &NormalizedResourceSet,
    revision: ResourceRevision,
    mutation_id: MutationId,
) -> anyhow::Result<()> {
    let changed_keys: BTreeSet<_> = changes
        .iter()
        .filter(|change| {
            matches!(
                change.action,
                ResourceChangeAction::Create | ResourceChangeAction::Update
            )
        })
        .map(|change| change.key.clone())
        .collect();
    // Normalized resources are dependency ordered. Upsert new parents and
    // retarget children before deleting obsolete rows, so provider or
    // credential renames never violate the foreign-key graph mid-transaction.
    for resource in desired.resources() {
        if changed_keys.contains(&resource.key()) {
            write_resource(connection, resource, revision, mutation_id).await?;
        }
    }
    for kind in [
        ResourceKind::Mount,
        ResourceKind::Attachment,
        ResourceKind::Credential,
        ResourceKind::Provider,
    ] {
        for change in changes
            .iter()
            .filter(|change| change.action == ResourceChangeAction::Delete)
            .filter(|change| change.key.kind == kind)
        {
            delete_resource(connection, &change.key.name, kind).await?;
        }
    }
    Ok(())
}

async fn write_resource(
    connection: &mut SqliteConnection,
    resource: &ResourceDefinition,
    revision: ResourceRevision,
    mutation_id: MutationId,
) -> anyhow::Result<()> {
    let revision = sql_int(revision.get(), "resource revision")?;
    match resource {
        ResourceDefinition::Provider(definition) => {
            write_provider(connection, definition, revision, mutation_id).await
        },
        ResourceDefinition::Credential(definition) => {
            write_credential(connection, definition, revision, mutation_id).await
        },
        ResourceDefinition::Mount(definition) => {
            write_mount(connection, definition, revision, mutation_id).await
        },
        ResourceDefinition::Attachment(definition) => {
            write_attachment(connection, definition, revision, mutation_id).await
        },
    }
}

async fn write_provider(
    connection: &mut SqliteConnection,
    definition: &ProviderDefinition,
    revision: i64,
    mutation_id: MutationId,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO provider_resources(\
             name, provider_digest, revision, last_mutation_id, updated_at\
         ) VALUES (?1, ?2, ?3, ?4, unixepoch()) \
         ON CONFLICT(name) DO UPDATE SET \
             provider_digest = excluded.provider_digest, \
             revision = excluded.revision, \
             last_mutation_id = excluded.last_mutation_id, \
             updated_at = excluded.updated_at",
    )
    .bind(definition.name.as_str())
    .bind(definition.artifact.as_bytes().as_slice())
    .bind(revision)
    .bind(mutation_id.as_bytes().as_slice())
    .execute(connection)
    .await
    .with_context(|| format!("write provider resource `{}`", definition.name))?;
    Ok(())
}

async fn write_credential(
    connection: &mut SqliteConnection,
    definition: &CredentialDefinition,
    revision: i64,
    mutation_id: MutationId,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO credential_resources(\
             name, provider_name, scheme, account, revision, last_mutation_id, updated_at\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch()) \
         ON CONFLICT(name) DO UPDATE SET \
             provider_name = excluded.provider_name, scheme = excluded.scheme, \
             account = excluded.account, revision = excluded.revision, \
             last_mutation_id = excluded.last_mutation_id, \
             updated_at = excluded.updated_at",
    )
    .bind(definition.name.as_str())
    .bind(definition.provider.as_str())
    .bind(&definition.scheme)
    .bind(&definition.account)
    .bind(revision)
    .bind(mutation_id.as_bytes().as_slice())
    .execute(connection)
    .await
    .with_context(|| format!("write credential resource `{}`", definition.name))?;
    Ok(())
}

async fn write_mount(
    connection: &mut SqliteConnection,
    definition: &MountResourceDefinition,
    revision: i64,
    mutation_id: MutationId,
) -> anyhow::Result<()> {
    let (canonical, version) = encode_mount(definition)?;
    sqlx::query(
        "INSERT INTO mount_resources(\
             name, canonical, version, provider_name, credential_name, \
             revision, last_mutation_id, updated_at\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, unixepoch()) \
         ON CONFLICT(name) DO UPDATE SET \
             canonical = excluded.canonical, version = excluded.version, \
             provider_name = excluded.provider_name, \
             credential_name = excluded.credential_name, \
             revision = excluded.revision, \
             last_mutation_id = excluded.last_mutation_id, \
             updated_at = excluded.updated_at",
    )
    .bind(definition.name.as_str())
    .bind(canonical)
    .bind(version.as_slice())
    .bind(definition.provider.as_str())
    .bind(definition.credential.as_ref().map(ResourceName::as_str))
    .bind(revision)
    .bind(mutation_id.as_bytes().as_slice())
    .execute(connection)
    .await
    .with_context(|| format!("write mount resource `{}`", definition.name))?;
    Ok(())
}

async fn write_attachment(
    connection: &mut SqliteConnection,
    definition: &AttachmentDefinition,
    revision: i64,
    mutation_id: MutationId,
) -> anyhow::Result<()> {
    let (canonical, version) = encode_attachment(definition)?;
    sqlx::query(
        "INSERT INTO attachment_resources(\
             name, canonical, version, revision, last_mutation_id, updated_at\
         ) VALUES (?1, ?2, ?3, ?4, ?5, unixepoch()) \
         ON CONFLICT(name) DO UPDATE SET \
             canonical = excluded.canonical, version = excluded.version, \
             revision = excluded.revision, \
             last_mutation_id = excluded.last_mutation_id, \
             updated_at = excluded.updated_at",
    )
    .bind(definition.name.as_str())
    .bind(&canonical)
    .bind(version.as_bytes().as_slice())
    .bind(revision)
    .bind(mutation_id.as_bytes().as_slice())
    .execute(&mut *connection)
    .await
    .with_context(|| format!("write attachment resource `{}`", definition.name))?;
    sqlx::query(
        "INSERT INTO attachment_instances(\
             name, desired_version, desired_spec, observed_version, observed_spec, phase, \
             runtime_instance, action_generation, last_error_code, last_error_detail, \
             retry_at, deleting, updated_at\
         ) VALUES (?1, ?2, ?3, NULL, NULL, 'pending', NULL, 0, NULL, NULL, NULL, 0, unixepoch()) \
         ON CONFLICT(name) DO UPDATE SET \
             desired_version = excluded.desired_version, desired_spec = excluded.desired_spec, \
             phase = CASE WHEN attachment_instances.observed_version = excluded.desired_version \
                 THEN 'ready' ELSE 'pending' END, \
             last_error_code = NULL, last_error_detail = NULL, retry_at = NULL, \
             deleting = 0, updated_at = excluded.updated_at",
    )
    .bind(definition.name.as_str())
    .bind(version.as_bytes().as_slice())
    .bind(canonical)
    .execute(connection)
    .await
    .with_context(|| format!("initialize observed attachment state `{}`", definition.name))?;
    Ok(())
}

async fn delete_resource(
    connection: &mut SqliteConnection,
    name: &ResourceName,
    kind: ResourceKind,
) -> anyhow::Result<()> {
    if kind == ResourceKind::Attachment {
        sqlx::query(
            "UPDATE attachment_instances \
             SET desired_version = NULL, desired_spec = NULL, phase = 'deleting', \
                 deleting = 1, last_error_code = NULL, last_error_detail = NULL, \
                 retry_at = NULL, updated_at = unixepoch() \
             WHERE name = ?1",
        )
        .bind(name.as_str())
        .execute(&mut *connection)
        .await
        .with_context(|| format!("mark attachment resource `{name}` deleting"))?;
    }
    let statement = match kind {
        ResourceKind::Provider => "DELETE FROM provider_resources WHERE name = ?1",
        ResourceKind::Credential => "DELETE FROM credential_resources WHERE name = ?1",
        ResourceKind::Mount => "DELETE FROM mount_resources WHERE name = ?1",
        ResourceKind::Attachment => "DELETE FROM attachment_resources WHERE name = ?1",
    };
    sqlx::query(statement)
        .bind(name.as_str())
        .execute(connection)
        .await
        .with_context(|| format!("delete {kind} resource `{name}`"))?;
    Ok(())
}

fn validate_secret_sidecars(
    desired: &NormalizedResourceSet,
    sidecars: &[CredentialSecretSidecar],
) -> Result<(), ResourceApplyError> {
    let providers: BTreeMap<_, _> = desired
        .resources()
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Provider(definition) => Some((definition.name.clone(), definition)),
            _ => None,
        })
        .collect();
    let credentials: BTreeMap<_, _> = desired
        .resources()
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Credential(definition) => {
                Some((definition.name.clone(), definition))
            },
            _ => None,
        })
        .collect();
    let mut seen = BTreeSet::new();
    for sidecar in sidecars {
        let invalid = |detail: String| ResourceApplyError::InvalidCredentialSecret {
            credential: sidecar.credential.clone(),
            detail,
        };
        if !seen.insert(sidecar.credential.clone()) {
            return Err(invalid("duplicate sidecar target".to_owned()));
        }
        let definition = credentials
            .get(&sidecar.credential)
            .ok_or_else(|| invalid("target credential resource is absent".to_owned()))?;
        let provider = providers
            .get(&definition.provider)
            .ok_or_else(|| invalid("target provider resource is absent".to_owned()))?;
        if sidecar.document.provider != provider.artifact {
            return Err(invalid(
                "credential material provider digest does not match the resource".to_owned(),
            ));
        }
        if sidecar.document.id.scheme() != definition.scheme
            || sidecar.document.id.account() != definition.account
        {
            return Err(invalid(
                "credential material identity does not match the resource".to_owned(),
            ));
        }
    }
    Ok(())
}

fn apply_input_digest(request: &ResourceApplyRequest) -> ResourceDigest {
    let mut targets: Vec<_> = request
        .credential_secrets
        .iter()
        .map(|sidecar| sidecar.credential.as_str())
        .collect();
    targets.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    hasher.update(APPLY_INPUT_DOMAIN);
    hasher.update(request.base_revision.get().to_be_bytes().as_slice());
    hasher.update(request.expected_desired_digest.as_bytes());
    hasher.update(
        u64::try_from(targets.len())
            .expect("sidecar count fits u64")
            .to_be_bytes()
            .as_slice(),
    );
    for target in targets {
        hasher.update(
            u64::try_from(target.len())
                .expect("resource name length fits u64")
                .to_be_bytes()
                .as_slice(),
        );
        hasher.update(target.as_bytes());
    }
    ResourceDigest::from_bytes(*hasher.finalize().as_bytes())
}

async fn existing_receipt(
    connection: &mut SqliteConnection,
    mutation_id: MutationId,
    input_digest: ResourceDigest,
) -> Result<Option<ApplyReceipt>, ResourceApplyError> {
    let Some(row) = sqlx::query(
        "SELECT input_digest, result_revision, result_digest, changed, \
                created, updated, deleted \
         FROM apply_receipts WHERE mutation_id = ?1",
    )
    .bind(mutation_id.as_bytes().as_slice())
    .fetch_optional(connection)
    .await
    .context("read resource apply receipt")?
    else {
        return Ok(None);
    };
    let stored_input = ResourceDigest::from_bytes(row.digest("input_digest")?);
    if stored_input != input_digest {
        return Err(ResourceApplyError::MutationIdReuse(mutation_id));
    }
    Ok(Some(decode_receipt(&row, mutation_id)?))
}

async fn write_receipt(
    connection: &mut SqliteConnection,
    input_digest: ResourceDigest,
    receipt: &ApplyReceipt,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO apply_receipts(\
             mutation_id, input_digest, result_revision, result_digest, changed, \
             created, updated, deleted, created_at\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, unixepoch())",
    )
    .bind(receipt.mutation_id.as_bytes().as_slice())
    .bind(input_digest.as_bytes().as_slice())
    .bind(sql_int(receipt.revision.get(), "receipt revision")?)
    .bind(receipt.desired_digest.as_bytes().as_slice())
    .bind(i64::from(receipt.changed))
    .bind(i64::from(receipt.created))
    .bind(i64::from(receipt.updated))
    .bind(i64::from(receipt.deleted))
    .execute(&mut *connection)
    .await
    .context("store resource apply receipt")?;
    sqlx::query(
        "DELETE FROM apply_receipts \
         WHERE rowid NOT IN (\
             SELECT rowid FROM apply_receipts \
             ORDER BY created_at DESC, rowid DESC LIMIT ?1\
         )",
    )
    .bind(APPLY_RECEIPT_LIMIT)
    .execute(connection)
    .await
    .context("prune resource apply receipts")?;
    Ok(())
}

fn decode_receipt(row: &SqliteRow, mutation_id: MutationId) -> anyhow::Result<ApplyReceipt> {
    Ok(ApplyReceipt {
        mutation_id,
        revision: ResourceRevision::new(row.unsigned("result_revision")?),
        desired_digest: ResourceDigest::from_bytes(row.digest("result_digest")?),
        changed: row.unsigned("changed")? == 1,
        created: u32::try_from(row.unsigned("created")?)
            .context("stored receipt created count exceeds u32")?,
        updated: u32::try_from(row.unsigned("updated")?)
            .context("stored receipt updated count exceeds u32")?,
        deleted: u32::try_from(row.unsigned("deleted")?)
            .context("stored receipt deleted count exceeds u32")?,
    })
}

fn count_changes(
    changes: &[omnifs_api::ResourceChange],
    action: ResourceChangeAction,
) -> anyhow::Result<u32> {
    u32::try_from(
        changes
            .iter()
            .filter(|change| change.action == action)
            .count(),
    )
    .context("resource change count exceeds u32")
}

async fn resource_initialized(connection: &mut SqliteConnection) -> anyhow::Result<bool> {
    let initialized: i64 =
        sqlx::query_scalar("SELECT initialized FROM resource_state WHERE singleton = 1")
            .fetch_one(connection)
            .await
            .context("read resource initialization state")?;
    match initialized {
        0 => Ok(false),
        1 => Ok(true),
        value => anyhow::bail!("stored resource initialized flag is invalid: {value}"),
    }
}

async fn legacy_resource_definitions(
    connection: &mut SqliteConnection,
) -> anyhow::Result<Vec<ResourceDefinition>> {
    let provider_rows = sqlx::query_as::<_, (Vec<u8>, String)>(
        "SELECT DISTINCT providers.digest, providers.name \
         FROM providers \
         JOIN (\
             SELECT provider_digest FROM mounts \
             UNION \
             SELECT provider_digest FROM credentials WHERE status <> 'deleted'\
         ) used ON used.provider_digest = providers.digest \
         ORDER BY providers.name, providers.digest",
    )
    .fetch_all(&mut *connection)
    .await
    .context("read providers for resource backfill")?;
    let credentials = sqlx::query_as::<_, CredentialSummary>(credential_summaries_query!(
        "WHERE status <> 'deleted' ORDER BY provider_name, scheme, account"
    ))
    .fetch_all(&mut *connection)
    .await
    .context("read credentials for resource backfill")?;
    let mounts = sqlx::query_as::<_, StoredMount>(mounts_query!("ORDER BY name"))
        .fetch_all(&mut *connection)
        .await
        .context("read mounts for resource backfill")?;

    let (mut definitions, provider_names) = migrated_providers(provider_rows)?;
    let (credential_definitions, credential_names) =
        migrated_credentials(credentials, &provider_names)?;
    definitions.extend(credential_definitions);
    definitions.extend(migrated_mounts(mounts, &provider_names, &credential_names)?);
    Ok(definitions)
}

type ProviderNameMap = BTreeMap<[u8; 32], ResourceName>;

fn migrated_providers(
    provider_rows: Vec<(Vec<u8>, String)>,
) -> anyhow::Result<(Vec<ResourceDefinition>, ProviderNameMap)> {
    let mut definitions = Vec::with_capacity(provider_rows.len());
    let mut names = BTreeMap::new();
    let mut used_provider_names = BTreeSet::new();
    let mut metadata_counts = BTreeMap::<String, usize>::new();
    for (_, metadata_name) in &provider_rows {
        *metadata_counts.entry(metadata_name.clone()).or_default() += 1;
    }
    for (digest, metadata_name) in provider_rows {
        let digest: [u8; 32] = digest.try_into().map_err(|bytes: Vec<u8>| {
            anyhow::anyhow!(
                "legacy provider `{metadata_name}` digest has {} bytes",
                bytes.len()
            )
        })?;
        let provider_id = ProviderId::from_digest(digest);
        let direct = metadata_counts.get(&metadata_name) == Some(&1)
            && ResourceName::new(metadata_name.clone()).is_ok()
            && !used_provider_names.contains(&ResourceName::new(metadata_name.clone())?);
        let name = if direct {
            ResourceName::new(metadata_name.clone())?
        } else {
            unique_suffixed_name(
                &metadata_name,
                provider_id.as_bytes(),
                "provider",
                &used_provider_names,
            )?
        };
        used_provider_names.insert(name.clone());
        names.insert(*provider_id.as_bytes(), name.clone());
        definitions.push(ResourceDefinition::Provider(ProviderDefinition {
            name,
            artifact: provider_id,
        }));
    }
    Ok((definitions, names))
}

type CredentialNameMap = BTreeMap<(String, String, String), ResourceName>;

fn migrated_credentials(
    credentials: Vec<CredentialSummary>,
    provider_names: &ProviderNameMap,
) -> anyhow::Result<(Vec<ResourceDefinition>, CredentialNameMap)> {
    let mut definitions = Vec::with_capacity(credentials.len());
    let mut credential_names = BTreeMap::new();
    let mut used_credential_names = BTreeSet::new();
    let mut account_counts = BTreeMap::<String, usize>::new();
    for credential in &credentials {
        *account_counts
            .entry(credential.id.account().to_owned())
            .or_default() += 1;
    }
    for credential in credentials {
        let provider = provider_names
            .get(credential.provider.as_bytes())
            .cloned()
            .with_context(|| {
                format!(
                    "credential {} has no retained provider resource",
                    credential.id
                )
            })?;
        let account = credential.id.account().to_owned();
        let direct = account_counts.get(&account) == Some(&1)
            && ResourceName::new(account.clone()).is_ok()
            && !used_credential_names.contains(&ResourceName::new(account.clone())?);
        let name = if direct {
            ResourceName::new(account.clone())?
        } else {
            let digest = credential_name_digest(
                credential.id.provider_name(),
                credential.id.scheme(),
                credential.id.account(),
            );
            unique_suffixed_name("credential", &digest, "credential", &used_credential_names)?
        };
        used_credential_names.insert(name.clone());
        credential_names.insert(
            (
                credential.id.provider_name().to_owned(),
                credential.id.scheme().to_owned(),
                credential.id.account().to_owned(),
            ),
            name.clone(),
        );
        definitions.push(ResourceDefinition::Credential(CredentialDefinition {
            name,
            provider,
            scheme: credential.id.scheme().to_owned(),
            account,
        }));
    }
    Ok((definitions, credential_names))
}

fn migrated_mounts(
    mounts: Vec<StoredMount>,
    provider_names: &ProviderNameMap,
    credential_names: &CredentialNameMap,
) -> anyhow::Result<Vec<ResourceDefinition>> {
    let mut definitions = Vec::with_capacity(mounts.len());
    for mount in mounts {
        let provider = provider_names
            .get(mount.document.provider.id.as_bytes())
            .cloned()
            .with_context(|| {
                format!(
                    "mount `{}` has no retained provider resource",
                    mount.document.name
                )
            })?;
        let credential = mount
            .document
            .credential
            .as_ref()
            .map(|credential| {
                credential_names
                    .get(&(
                        credential.provider_name().to_owned(),
                        credential.scheme().to_owned(),
                        credential.account().to_owned(),
                    ))
                    .cloned()
                    .with_context(|| {
                        format!(
                            "mount `{}` has no active credential resource",
                            mount.document.name
                        )
                    })
            })
            .transpose()?;
        definitions.push(ResourceDefinition::Mount(MountResourceDefinition {
            name: ResourceName::new(mount.document.name.to_string())?,
            provider,
            credential,
            config: mount.document.config,
            limits: mount.document.limits.map(
                |MountLimits {
                     max_memory_mb,
                     max_fetch_blob_bytes,
                 }| ResourceLimits {
                    max_memory_mb,
                    max_fetch_blob_bytes,
                },
            ),
        }));
    }
    Ok(definitions)
}

fn unique_suffixed_name(
    source: &str,
    digest: &[u8; 32],
    fallback: &str,
    used: &BTreeSet<ResourceName>,
) -> anyhow::Result<ResourceName> {
    let mut base = String::new();
    let mut last_dash = false;
    for character in source.chars().flat_map(char::to_lowercase) {
        let next = if character.is_ascii_lowercase() || character.is_ascii_digit() {
            last_dash = false;
            Some(character)
        } else if !last_dash && !base.is_empty() {
            last_dash = true;
            Some('-')
        } else {
            None
        };
        if let Some(next) = next {
            base.push(next);
        }
    }
    while base.ends_with('-') {
        base.pop();
    }
    if base.is_empty() {
        base.push_str(fallback);
    }
    let hex = hex::encode(digest);
    for prefix_length in (8..=hex.len()).step_by(2) {
        let max_base = 32_usize
            .checked_sub(prefix_length + 1)
            .context("resource digest suffix is too long")?;
        let mut truncated = base.chars().take(max_base).collect::<String>();
        while truncated.ends_with('-') {
            truncated.pop();
        }
        if truncated.is_empty() {
            truncated.push_str(fallback);
            truncated.truncate(max_base);
        }
        let candidate = ResourceName::new(format!("{truncated}-{}", &hex[..prefix_length]))?;
        if !used.contains(&candidate) {
            return Ok(candidate);
        }
    }
    anyhow::bail!("could not derive a unique resource name from `{source}`")
}

fn credential_name_digest(provider: &str, scheme: &str, account: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"omnifs-credential-resource-name-v1\0");
    for value in [provider, scheme, account] {
        hasher.update(
            u64::try_from(value.len())
                .expect("credential identity length fits u64")
                .to_be_bytes()
                .as_slice(),
        );
        hasher.update(value.as_bytes());
    }
    *hasher.finalize().as_bytes()
}
