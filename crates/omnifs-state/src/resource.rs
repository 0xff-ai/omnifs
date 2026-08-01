//! Durable desired resources and atomic full-set apply.

pub(crate) mod codec;

use crate::credential::CredentialDocument;
use crate::db::Db;
use crate::row::{RowExt as _, sql_int};
use anyhow::Context as _;
use omnifs_api::{
    ApplyReceipt, FilesystemDefinition, NormalizedResourceSet, ResourceChangeAction,
    ResourceDefinition, plan,
};
use omnifs_core::{
    FilesystemVersion, MutationId, ResourceDigest, ResourceKind, ResourceName, ResourceRevision,
};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnection, SqliteRow};
use std::collections::{BTreeMap, BTreeSet};

use codec::{decode_resources, encode_filesystem, encode_resources};

const APPLY_RECEIPT_LIMIT: i64 = 256;
const APPLY_INPUT_DOMAIN: &[u8] = b"omnifs-resource-apply-input-v1\0";

/// One transactionally consistent non-secret desired-state head.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceSnapshot {
    pub revision: ResourceRevision,
    pub desired_digest: ResourceDigest,
    pub resources: NormalizedResourceSet,
}

struct StoredResources {
    snapshot: ResourceSnapshot,
    revisions: BTreeMap<omnifs_core::ResourceKey, ResourceRevision>,
}

/// One exact desired filesystem row with its durable content version and
/// resource revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredFilesystem {
    pub definition: omnifs_api::FilesystemDefinition,
    pub version: FilesystemVersion,
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

        let current = read_stored_resources(self.raw()).await?;
        if current.snapshot.desired_digest == request.desired.digest() {
            let receipt = ApplyReceipt {
                mutation_id: request.mutation_id,
                revision: current.snapshot.revision,
                desired_digest: current.snapshot.desired_digest,
                created: 0,
                updated: 0,
                deleted: 0,
                changed: false,
            };
            write_receipt(self.raw(), input_digest, &receipt).await?;
            return Ok(receipt);
        }
        if current.snapshot.revision != request.base_revision {
            return Err(ResourceApplyError::StaleRevision {
                expected: request.base_revision,
                actual: current.snapshot.revision,
            });
        }

        let changes = plan(&current.snapshot.resources, &request.desired);
        let created = count_changes(&changes, ResourceChangeAction::Create)?;
        let updated = count_changes(&changes, ResourceChangeAction::Update)?;
        let deleted = count_changes(&changes, ResourceChangeAction::Delete)?;
        let revision = current
            .snapshot
            .revision
            .next()
            .context("resource revision exhausted")?;

        reconcile_filesystem_instances(self.raw(), &changes, &request.desired).await?;
        for sidecar in request.credential_secrets {
            self.submit_credential_row(sidecar.document)
                .await
                .map_err(|error| anyhow::anyhow!(error))?;
        }

        let changed_keys = changes
            .iter()
            .filter(|change| {
                matches!(
                    change.action,
                    ResourceChangeAction::Create | ResourceChangeAction::Update
                )
            })
            .map(|change| change.key.clone())
            .collect::<BTreeSet<_>>();
        let stored = request
            .desired
            .resources()
            .iter()
            .cloned()
            .map(|resource| {
                let key = resource.key();
                let resource_revision = if changed_keys.contains(&key) {
                    revision
                } else {
                    current
                        .revisions
                        .get(&key)
                        .copied()
                        .context("unchanged resource has no stored revision")?
                };
                Ok((resource, resource_revision))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let canonical = encode_resources(stored)?;
        sqlx::query(
            "UPDATE resource_state \
             SET revision = ?1, desired_digest = ?2, resources = ?3, updated_at = unixepoch() \
             WHERE singleton = 1",
        )
        .bind(sql_int(revision.get(), "resource revision")?)
        .bind(request.desired.digest().as_bytes().as_slice())
        .bind(canonical)
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

pub(crate) async fn read_resource_snapshot(
    connection: &mut SqliteConnection,
) -> anyhow::Result<ResourceSnapshot> {
    Ok(read_stored_resources(connection).await?.snapshot)
}

async fn read_stored_resources(
    connection: &mut SqliteConnection,
) -> anyhow::Result<StoredResources> {
    let (revision, digest, canonical) = sqlx::query_as::<_, (i64, Vec<u8>, Vec<u8>)>(
        "SELECT revision, desired_digest, resources \
         FROM resource_state WHERE singleton = 1",
    )
    .fetch_one(&mut *connection)
    .await
    .context("read desired resources")?;
    let revision =
        ResourceRevision::new(u64::try_from(revision).context("resource revision is negative")?);
    let desired_digest =
        ResourceDigest::from_bytes(digest.try_into().map_err(|bytes: Vec<u8>| {
            anyhow::anyhow!(
                "stored resource digest has {} bytes; expected 32",
                bytes.len()
            )
        })?);
    let decoded = decode_resources(&canonical)?;
    let revisions = decoded
        .iter()
        .map(|(resource, revision)| (resource.key(), *revision))
        .collect();
    let resources =
        NormalizedResourceSet::new(decoded.into_iter().map(|(resource, _)| resource).collect())
            .context("validate stored desired resources")?;
    anyhow::ensure!(
        resources.digest() == desired_digest,
        "stored desired resource digest does not match resource bytes"
    );
    Ok(StoredResources {
        snapshot: ResourceSnapshot {
            revision,
            desired_digest,
            resources,
        },
        revisions,
    })
}

pub(crate) async fn desired_filesystems(
    pool: &SqlitePool,
) -> anyhow::Result<Vec<DesiredFilesystem>> {
    let mut connection = pool.acquire().await.context("acquire desired resources")?;
    let stored = read_stored_resources(&mut connection).await?;
    stored
        .snapshot
        .resources
        .resources()
        .iter()
        .filter_map(|resource| match resource {
            ResourceDefinition::Filesystem(definition) => Some(definition.clone()),
            _ => None,
        })
        .map(|definition| {
            let revision = stored
                .revisions
                .get(&definition.key())
                .copied()
                .context("desired filesystem has no stored revision")?;
            let (_, version) = encode_filesystem(&definition)?;
            Ok(DesiredFilesystem {
                definition,
                version,
                revision,
            })
        })
        .collect()
}

async fn reconcile_filesystem_instances(
    connection: &mut SqliteConnection,
    changes: &[omnifs_api::ResourceChange],
    desired: &NormalizedResourceSet,
) -> anyhow::Result<()> {
    let changed_filesystem_names = changes
        .iter()
        .filter(|change| {
            change.key.kind == ResourceKind::Filesystem
                && matches!(
                    change.action,
                    ResourceChangeAction::Create | ResourceChangeAction::Update
                )
        })
        .map(|change| change.key.name.clone())
        .collect::<BTreeSet<_>>();
    for definition in desired.resources().iter().filter_map(|resource| {
        let ResourceDefinition::Filesystem(definition) = resource else {
            return None;
        };
        changed_filesystem_names
            .contains(&definition.name)
            .then_some(definition)
    }) {
        upsert_filesystem_instance(connection, definition).await?;
    }
    for name in changes.iter().filter_map(|change| {
        (change.key.kind == ResourceKind::Filesystem
            && change.action == ResourceChangeAction::Delete)
            .then_some(&change.key.name)
    }) {
        sqlx::query(
            "UPDATE filesystem_instances \
             SET desired_version = NULL, desired_spec = NULL, phase = 'deleting', \
                 deleting = 1, last_error_code = NULL, last_error_detail = NULL, \
                 retry_at = NULL, updated_at = unixepoch() \
             WHERE name = ?1",
        )
        .bind(name.as_str())
        .execute(&mut *connection)
        .await
        .with_context(|| format!("mark filesystem resource `{name}` deleting"))?;
    }
    Ok(())
}

async fn upsert_filesystem_instance(
    connection: &mut SqliteConnection,
    definition: &FilesystemDefinition,
) -> anyhow::Result<()> {
    let (canonical, version) = encode_filesystem(definition)?;
    sqlx::query(
        "INSERT INTO filesystem_instances(\
             name, desired_version, desired_spec, observed_version, observed_spec, phase, \
             runtime_instance, action_generation, last_error_code, last_error_detail, \
             retry_at, deleting, updated_at\
         ) VALUES (?1, ?2, ?3, NULL, NULL, 'pending', NULL, 0, NULL, NULL, NULL, 0, unixepoch()) \
         ON CONFLICT(name) DO UPDATE SET \
             desired_version = excluded.desired_version, desired_spec = excluded.desired_spec, \
             phase = CASE WHEN filesystem_instances.observed_version = excluded.desired_version \
                 THEN 'ready' ELSE 'pending' END, \
             last_error_code = NULL, last_error_detail = NULL, retry_at = NULL, \
             deleting = 0, updated_at = excluded.updated_at",
    )
    .bind(definition.name.as_str())
    .bind(version.as_bytes().as_slice())
    .bind(canonical)
    .execute(connection)
    .await
    .with_context(|| format!("initialize observed filesystem state `{}`", definition.name))?;
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
