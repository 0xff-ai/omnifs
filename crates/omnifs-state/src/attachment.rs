//! Durable observed state for daemon-owned attachment runtimes.

use crate::db::Db;
use crate::row::{RowExt as _, sql_int};
use anyhow::Context as _;
use omnifs_api::AttachmentDefinition;
use omnifs_core::{AttachmentSpec, AttachmentVersion, ResourceName};
use sqlx::sqlite::{SqliteConnection, SqliteRow};
use sqlx::{Row as _, SqlitePool};

/// Closed lifecycle stages persisted for one attachment runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentPhase {
    Pending,
    WaitingForNamespace,
    Starting,
    Ready,
    Stopping,
    Retrying,
    Failed,
    Deleting,
}

impl AttachmentPhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::WaitingForNamespace => "waiting_for_namespace",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::Retrying => "retrying",
            Self::Failed => "failed",
            Self::Deleting => "deleting",
        }
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "waiting_for_namespace" => Ok(Self::WaitingForNamespace),
            "starting" => Ok(Self::Starting),
            "ready" => Ok(Self::Ready),
            "stopping" => Ok(Self::Stopping),
            "retrying" => Ok(Self::Retrying),
            "failed" => Ok(Self::Failed),
            "deleting" => Ok(Self::Deleting),
            other => anyhow::bail!("stored attachment phase `{other}` is not recognized"),
        }
    }
}

/// The durable identity and observed lifecycle state for one attachment.
///
/// A row may outlive its desired resource while deletion is in progress.  In
/// that case `desired_version` is absent and `deleting` remains true until the
/// supervisor has proved that the exact runtime is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentInstance {
    pub name: ResourceName,
    pub desired_version: Option<AttachmentVersion>,
    pub desired_spec: Option<AttachmentSpec>,
    pub observed_version: Option<AttachmentVersion>,
    pub observed_spec: Option<AttachmentSpec>,
    pub phase: AttachmentPhase,
    pub runtime_instance: Option<String>,
    pub action_generation: u64,
    pub last_error_code: Option<String>,
    pub last_error_detail: Option<String>,
    pub retry_at: Option<i64>,
    pub deleting: bool,
    pub updated_at: i64,
}

/// One fenced supervisor update to an attachment's observed lifecycle state.
///
/// Resource apply owns desired fields and deletion state. Durable action
/// acceptance owns `action_generation`. A supervisor carries all three facts
/// it observed before an effect, so a stale result cannot become visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentObservation {
    pub name: ResourceName,
    pub expected_desired_version: Option<AttachmentVersion>,
    pub expected_action_generation: u64,
    pub expected_runtime_instance: Option<String>,
    pub observed_version: Option<AttachmentVersion>,
    pub observed_spec: Option<AttachmentSpec>,
    pub phase: AttachmentPhase,
    pub runtime_instance: Option<String>,
    pub last_error_code: Option<String>,
    pub last_error_detail: Option<String>,
    pub retry_at: Option<i64>,
}

impl AttachmentObservation {
    /// Construct a fenced write from one exact durable row read before an
    /// attachment effect. Callers can change only observed lifecycle fields.
    #[must_use]
    pub fn from_instance(instance: &AttachmentInstance) -> Self {
        Self {
            name: instance.name.clone(),
            expected_desired_version: instance.desired_version,
            expected_action_generation: instance.action_generation,
            expected_runtime_instance: instance.runtime_instance.clone(),
            observed_version: instance.observed_version,
            observed_spec: instance.observed_spec.clone(),
            phase: instance.phase,
            runtime_instance: instance.runtime_instance.clone(),
            last_error_code: instance.last_error_code.clone(),
            last_error_detail: instance.last_error_detail.clone(),
            retry_at: instance.retry_at,
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.observed_version.is_some() == self.observed_spec.is_some(),
            "attachment observed version and spec presence differ"
        );
        validate_runtime_instance(
            self.expected_runtime_instance.as_deref(),
            "expected attachment runtime instance",
        )?;
        validate_runtime_instance(
            self.runtime_instance.as_deref(),
            "attachment runtime instance",
        )?;
        if let Some(retry_at) = self.retry_at {
            anyhow::ensure!(retry_at >= 0, "attachment retry_at is negative");
        }
        if let Some(code) = &self.last_error_code {
            anyhow::ensure!(!code.is_empty(), "attachment error code cannot be empty");
        }
        if let Some(detail) = &self.last_error_detail {
            anyhow::ensure!(
                !detail.is_empty(),
                "attachment error detail cannot be empty"
            );
        }
        if self.phase == AttachmentPhase::Ready {
            anyhow::ensure!(
                self.expected_desired_version.is_some()
                    && self.observed_version == self.expected_desired_version,
                "a ready attachment observation must match its expected desired version"
            );
        }
        Ok(())
    }
}

impl AttachmentInstance {
    #[must_use]
    pub fn pending(name: ResourceName) -> Self {
        Self {
            name,
            desired_version: None,
            desired_spec: None,
            observed_version: None,
            observed_spec: None,
            phase: AttachmentPhase::Pending,
            runtime_instance: None,
            action_generation: 0,
            last_error_code: None,
            last_error_detail: None,
            retry_at: None,
            deleting: false,
            updated_at: 0,
        }
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.updated_at >= 0, "attachment updated_at is negative");
        anyhow::ensure!(
            self.desired_version.is_some() == self.desired_spec.is_some(),
            "attachment desired version and spec presence differ"
        );
        anyhow::ensure!(
            self.observed_version.is_some() == self.observed_spec.is_some(),
            "attachment observed version and spec presence differ"
        );
        validate_runtime_instance(
            self.runtime_instance.as_deref(),
            "attachment runtime instance",
        )?;
        if let Some(retry_at) = self.retry_at {
            anyhow::ensure!(retry_at >= 0, "attachment retry_at is negative");
        }
        if let Some(code) = &self.last_error_code {
            anyhow::ensure!(!code.is_empty(), "attachment error code cannot be empty");
        }
        if let Some(detail) = &self.last_error_detail {
            anyhow::ensure!(
                !detail.is_empty(),
                "attachment error detail cannot be empty"
            );
        }
        Ok(())
    }
}

impl Db<'_> {
    pub(crate) async fn write_attachment_observation(
        &mut self,
        observation: AttachmentObservation,
    ) -> anyhow::Result<Option<AttachmentInstance>> {
        observation.validate()?;
        self.transact("attachment observation", async move |db| {
            let result = sqlx::query(
                "UPDATE attachment_instances SET \
                     observed_version = ?1, observed_spec = ?2, phase = ?3, \
                     runtime_instance = ?4, last_error_code = ?5, last_error_detail = ?6, \
                     retry_at = ?7, updated_at = unixepoch() \
                 WHERE name = ?8 \
                   AND ((desired_version IS NULL AND ?9 IS NULL) OR desired_version = ?9) \
                   AND action_generation = ?10 \
                   AND ((runtime_instance IS NULL AND ?11 IS NULL) OR runtime_instance = ?11)",
            )
            .bind(
                observation
                    .observed_version
                    .map(|version| version.as_bytes().to_vec()),
            )
            .bind(encode_spec(
                &observation.name,
                observation.observed_spec.as_ref(),
                observation.observed_version,
            )?)
            .bind(observation.phase.as_str())
            .bind(observation.runtime_instance.as_deref())
            .bind(observation.last_error_code.as_deref())
            .bind(observation.last_error_detail.as_deref())
            .bind(observation.retry_at)
            .bind(observation.name.as_str())
            .bind(
                observation
                    .expected_desired_version
                    .map(|version| version.as_bytes().to_vec()),
            )
            .bind(sql_int(
                observation.expected_action_generation,
                "expected attachment action generation",
            )?)
            .bind(observation.expected_runtime_instance.as_deref())
            .execute(db.raw())
            .await
            .with_context(|| format!("write attachment observation `{}`", observation.name))?;
            if result.rows_affected() == 0 {
                return Ok(None);
            }
            load_instance(db.raw(), &observation.name)
                .await?
                .map(Some)
                .context("attachment instance disappeared after observation write")
        })
        .await
    }

    pub(crate) async fn delete_attachment_instance_if_deleting(
        &mut self,
        name: ResourceName,
        runtime_instance: Option<String>,
    ) -> anyhow::Result<bool> {
        self.transact(
            "conditional attachment instance deletion",
            async move |db| {
                let result = sqlx::query(
                    "DELETE FROM attachment_instances \
                 WHERE name = ?1 AND desired_version IS NULL AND deleting = 1 \
                   AND ((runtime_instance IS NULL AND ?2 IS NULL) OR runtime_instance = ?2)",
                )
                .bind(name.as_str())
                .bind(runtime_instance.as_deref())
                .execute(db.raw())
                .await
                .with_context(|| format!("conditionally delete attachment instance `{name}`"))?;
                Ok(result.rows_affected() == 1)
            },
        )
        .await
    }
}

pub(crate) async fn load_instance(
    connection: &mut SqliteConnection,
    name: &ResourceName,
) -> anyhow::Result<Option<AttachmentInstance>> {
    sqlx::query(
        "SELECT name, desired_version, desired_spec, observed_version, observed_spec, phase, runtime_instance, \
                action_generation, last_error_code, last_error_detail, retry_at, deleting, updated_at \
         FROM attachment_instances WHERE name = ?1",
    )
    .bind(name.as_str())
    .fetch_optional(connection)
    .await
    .context("read attachment instance")?
    .map(|row| decode_instance(&row))
    .transpose()
}

pub(crate) async fn list_instances(pool: &SqlitePool) -> anyhow::Result<Vec<AttachmentInstance>> {
    sqlx::query(
        "SELECT name, desired_version, desired_spec, observed_version, observed_spec, phase, runtime_instance, \
                action_generation, last_error_code, last_error_detail, retry_at, deleting, updated_at \
         FROM attachment_instances ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .context("list attachment instances")?
    .iter()
    .map(decode_instance)
    .collect()
}

fn decode_instance(row: &SqliteRow) -> anyhow::Result<AttachmentInstance> {
    let name_text = row.text("name")?;
    let name = ResourceName::new(name_text.clone())
        .with_context(|| format!("decode attachment instance name `{name_text}`"))?;
    let phase_text = row.text("phase")?;
    let deleting: i64 = row
        .try_get("deleting")
        .context("read attachment deletion flag")?;
    let deleting = match deleting {
        0 => false,
        1 => true,
        value => anyhow::bail!("stored attachment deletion flag is {value}, expected 0 or 1"),
    };
    let updated_at: i64 = row
        .try_get("updated_at")
        .context("read attachment updated_at")?;
    let action_generation = row.unsigned("action_generation")?;
    let retry_at: Option<i64> = row
        .try_get("retry_at")
        .context("read attachment retry_at")?;
    if retry_at.is_some_and(|value| value < 0) {
        anyhow::bail!("stored attachment retry_at is negative");
    }
    let runtime_instance: Option<String> = row
        .try_get("runtime_instance")
        .context("read attachment runtime instance")?;
    let instance = AttachmentInstance {
        name,
        desired_version: decode_optional_version(row, "desired_version")?,
        desired_spec: None,
        observed_version: decode_optional_version(row, "observed_version")?,
        observed_spec: None,
        phase: AttachmentPhase::parse(&phase_text)?,
        runtime_instance,
        action_generation,
        last_error_code: row.optional_text("last_error_code")?,
        last_error_detail: row.optional_text("last_error_detail")?,
        retry_at,
        deleting,
        updated_at,
    };
    let mut instance = instance;
    instance.desired_spec = decode_spec(
        &instance.name,
        row.optional_bytes("desired_spec")?.as_deref(),
        instance.desired_version,
    )?;
    instance.observed_spec = decode_spec(
        &instance.name,
        row.optional_bytes("observed_spec")?.as_deref(),
        instance.observed_version,
    )?;
    instance.validate()?;
    Ok(instance)
}

fn encode_spec(
    name: &ResourceName,
    spec: Option<&AttachmentSpec>,
    version: Option<AttachmentVersion>,
) -> anyhow::Result<Option<Vec<u8>>> {
    match (spec, version) {
        (None, None) => Ok(None),
        (Some(spec), Some(expected)) => {
            let definition = AttachmentDefinition {
                name: name.clone(),
                spec: spec.clone(),
            };
            let (canonical, actual) = crate::resource::codec::encode_attachment(&definition)?;
            anyhow::ensure!(
                actual == expected,
                "attachment spec version does not match canonical bytes"
            );
            Ok(Some(canonical))
        },
        _ => anyhow::bail!("attachment spec and version presence differ"),
    }
}

fn decode_spec(
    name: &ResourceName,
    canonical: Option<&[u8]>,
    version: Option<AttachmentVersion>,
) -> anyhow::Result<Option<AttachmentSpec>> {
    match (canonical, version) {
        (None, None) => Ok(None),
        (Some(canonical), Some(version)) => {
            let definition = crate::resource::codec::decode_attachment(canonical, version)?;
            anyhow::ensure!(
                definition.name == *name,
                "stored attachment instance spec name does not match row name"
            );
            Ok(Some(definition.spec))
        },
        _ => anyhow::bail!("stored attachment spec and version presence differ"),
    }
}

fn decode_optional_version(
    row: &SqliteRow,
    column: &str,
) -> anyhow::Result<Option<AttachmentVersion>> {
    row.optional_bytes(column)?
        .map(|bytes| {
            let digest: [u8; 32] = bytes.clone().try_into().map_err(|_| {
                anyhow::anyhow!(
                    "stored attachment `{column}` has {} bytes; expected 32",
                    bytes.len()
                )
            })?;
            Ok(AttachmentVersion::from_digest(digest))
        })
        .transpose()
}

fn validate_runtime_instance(value: Option<&str>, field: &str) -> anyhow::Result<()> {
    if let Some(instance) = value {
        omnifs_core::RuntimeInstanceId::new(instance.to_owned())
            .with_context(|| format!("{field} is invalid"))?;
    }
    Ok(())
}
