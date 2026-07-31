//! Durable observed state for daemon-owned filesystem runtimes.

use crate::db::Db;
use crate::row::{RowExt as _, sql_int};
use anyhow::Context as _;
use omnifs_api::FilesystemDefinition;
use omnifs_core::{FilesystemSpec, FilesystemVersion, ResourceName};
use sqlx::sqlite::{SqliteConnection, SqliteRow};
use sqlx::{Row as _, SqlitePool};

/// Closed lifecycle stages persisted for one filesystem runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemPhase {
    Pending,
    WaitingForNamespace,
    Starting,
    Ready,
    Stopping,
    Retrying,
    Failed,
    Deleting,
}

impl FilesystemPhase {
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
            other => anyhow::bail!("stored filesystem phase `{other}` is not recognized"),
        }
    }
}

/// The durable identity and observed lifecycle state for one filesystem.
///
/// A row may outlive its desired resource while deletion is in progress.  In
/// that case `desired_version` is absent and `deleting` remains true until the
/// supervisor has proved that the exact runtime is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemInstance {
    pub name: ResourceName,
    pub desired_version: Option<FilesystemVersion>,
    pub desired_spec: Option<FilesystemSpec>,
    pub observed_version: Option<FilesystemVersion>,
    pub observed_spec: Option<FilesystemSpec>,
    pub phase: FilesystemPhase,
    pub runtime_instance: Option<String>,
    pub action_generation: u64,
    pub last_error_code: Option<String>,
    pub last_error_detail: Option<String>,
    pub retry_at: Option<i64>,
    pub deleting: bool,
    pub updated_at: i64,
}

/// One fenced supervisor update to a filesystem's observed lifecycle state.
///
/// Resource apply owns desired fields and deletion state. Durable action
/// acceptance owns `action_generation`. A supervisor carries all three facts
/// it observed before an effect, so a stale result cannot become visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemObservation {
    pub name: ResourceName,
    pub expected_desired_version: Option<FilesystemVersion>,
    pub expected_action_generation: u64,
    pub expected_runtime_instance: Option<String>,
    pub observed_version: Option<FilesystemVersion>,
    pub observed_spec: Option<FilesystemSpec>,
    pub phase: FilesystemPhase,
    pub runtime_instance: Option<String>,
    pub last_error_code: Option<String>,
    pub last_error_detail: Option<String>,
    pub retry_at: Option<i64>,
}

impl FilesystemObservation {
    /// Construct a fenced write from one exact durable row read before an
    /// filesystem effect. Callers can change only observed lifecycle fields.
    #[must_use]
    pub fn from_instance(instance: &FilesystemInstance) -> Self {
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
            "filesystem observed version and spec presence differ"
        );
        validate_runtime_instance(
            self.expected_runtime_instance.as_deref(),
            "expected filesystem runtime instance",
        )?;
        validate_runtime_instance(
            self.runtime_instance.as_deref(),
            "filesystem runtime instance",
        )?;
        if let Some(retry_at) = self.retry_at {
            anyhow::ensure!(retry_at >= 0, "filesystem retry_at is negative");
        }
        if let Some(code) = &self.last_error_code {
            anyhow::ensure!(!code.is_empty(), "filesystem error code cannot be empty");
        }
        if let Some(detail) = &self.last_error_detail {
            anyhow::ensure!(
                !detail.is_empty(),
                "filesystem error detail cannot be empty"
            );
        }
        if self.phase == FilesystemPhase::Ready {
            anyhow::ensure!(
                self.expected_desired_version.is_some()
                    && self.observed_version == self.expected_desired_version,
                "a ready filesystem observation must match its expected desired version"
            );
        }
        Ok(())
    }
}

impl FilesystemInstance {
    #[must_use]
    pub fn pending(name: ResourceName) -> Self {
        Self {
            name,
            desired_version: None,
            desired_spec: None,
            observed_version: None,
            observed_spec: None,
            phase: FilesystemPhase::Pending,
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
        anyhow::ensure!(self.updated_at >= 0, "filesystem updated_at is negative");
        anyhow::ensure!(
            self.desired_version.is_some() == self.desired_spec.is_some(),
            "filesystem desired version and spec presence differ"
        );
        anyhow::ensure!(
            self.observed_version.is_some() == self.observed_spec.is_some(),
            "filesystem observed version and spec presence differ"
        );
        validate_runtime_instance(
            self.runtime_instance.as_deref(),
            "filesystem runtime instance",
        )?;
        if let Some(retry_at) = self.retry_at {
            anyhow::ensure!(retry_at >= 0, "filesystem retry_at is negative");
        }
        if let Some(code) = &self.last_error_code {
            anyhow::ensure!(!code.is_empty(), "filesystem error code cannot be empty");
        }
        if let Some(detail) = &self.last_error_detail {
            anyhow::ensure!(
                !detail.is_empty(),
                "filesystem error detail cannot be empty"
            );
        }
        Ok(())
    }
}

impl Db<'_> {
    pub(crate) async fn write_filesystem_observation(
        &mut self,
        observation: FilesystemObservation,
    ) -> anyhow::Result<Option<FilesystemInstance>> {
        observation.validate()?;
        self.transact("filesystem observation", async move |db| {
            let result = sqlx::query(
                "UPDATE filesystem_instances SET \
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
                "expected filesystem action generation",
            )?)
            .bind(observation.expected_runtime_instance.as_deref())
            .execute(db.raw())
            .await
            .with_context(|| format!("write filesystem observation `{}`", observation.name))?;
            if result.rows_affected() == 0 {
                return Ok(None);
            }
            load_instance(db.raw(), &observation.name)
                .await?
                .map(Some)
                .context("filesystem instance disappeared after observation write")
        })
        .await
    }

    pub(crate) async fn delete_filesystem_instance_if_deleting(
        &mut self,
        name: ResourceName,
        runtime_instance: Option<String>,
    ) -> anyhow::Result<bool> {
        self.transact(
            "conditional filesystem instance deletion",
            async move |db| {
                let result = sqlx::query(
                    "DELETE FROM filesystem_instances \
                 WHERE name = ?1 AND desired_version IS NULL AND deleting = 1 \
                   AND ((runtime_instance IS NULL AND ?2 IS NULL) OR runtime_instance = ?2)",
                )
                .bind(name.as_str())
                .bind(runtime_instance.as_deref())
                .execute(db.raw())
                .await
                .with_context(|| format!("conditionally delete filesystem instance `{name}`"))?;
                Ok(result.rows_affected() == 1)
            },
        )
        .await
    }
}

pub(crate) async fn load_instance(
    connection: &mut SqliteConnection,
    name: &ResourceName,
) -> anyhow::Result<Option<FilesystemInstance>> {
    sqlx::query(
        "SELECT name, desired_version, desired_spec, observed_version, observed_spec, phase, runtime_instance, \
                action_generation, last_error_code, last_error_detail, retry_at, deleting, updated_at \
         FROM filesystem_instances WHERE name = ?1",
    )
    .bind(name.as_str())
    .fetch_optional(connection)
    .await
    .context("read filesystem instance")?
    .map(|row| decode_instance(&row))
    .transpose()
}

pub(crate) async fn list_instances(pool: &SqlitePool) -> anyhow::Result<Vec<FilesystemInstance>> {
    sqlx::query(
        "SELECT name, desired_version, desired_spec, observed_version, observed_spec, phase, runtime_instance, \
                action_generation, last_error_code, last_error_detail, retry_at, deleting, updated_at \
         FROM filesystem_instances ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .context("list filesystem instances")?
    .iter()
    .map(decode_instance)
    .collect()
}

fn decode_instance(row: &SqliteRow) -> anyhow::Result<FilesystemInstance> {
    let name_text = row.text("name")?;
    let name = ResourceName::new(name_text.clone())
        .with_context(|| format!("decode filesystem instance name `{name_text}`"))?;
    let phase_text = row.text("phase")?;
    let deleting: i64 = row
        .try_get("deleting")
        .context("read filesystem deletion flag")?;
    let deleting = match deleting {
        0 => false,
        1 => true,
        value => anyhow::bail!("stored filesystem deletion flag is {value}, expected 0 or 1"),
    };
    let updated_at: i64 = row
        .try_get("updated_at")
        .context("read filesystem updated_at")?;
    let action_generation = row.unsigned("action_generation")?;
    let retry_at: Option<i64> = row
        .try_get("retry_at")
        .context("read filesystem retry_at")?;
    if retry_at.is_some_and(|value| value < 0) {
        anyhow::bail!("stored filesystem retry_at is negative");
    }
    let runtime_instance: Option<String> = row
        .try_get("runtime_instance")
        .context("read filesystem runtime instance")?;
    let instance = FilesystemInstance {
        name,
        desired_version: decode_optional_version(row, "desired_version")?,
        desired_spec: None,
        observed_version: decode_optional_version(row, "observed_version")?,
        observed_spec: None,
        phase: FilesystemPhase::parse(&phase_text)?,
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
    spec: Option<&FilesystemSpec>,
    version: Option<FilesystemVersion>,
) -> anyhow::Result<Option<Vec<u8>>> {
    match (spec, version) {
        (None, None) => Ok(None),
        (Some(spec), Some(expected)) => {
            let definition = FilesystemDefinition {
                name: name.clone(),
                spec: spec.clone(),
            };
            let (canonical, actual) = crate::resource::codec::encode_filesystem(&definition)?;
            anyhow::ensure!(
                actual == expected,
                "filesystem spec version does not match canonical bytes"
            );
            Ok(Some(canonical))
        },
        _ => anyhow::bail!("filesystem spec and version presence differ"),
    }
}

fn decode_spec(
    name: &ResourceName,
    canonical: Option<&[u8]>,
    version: Option<FilesystemVersion>,
) -> anyhow::Result<Option<FilesystemSpec>> {
    match (canonical, version) {
        (None, None) => Ok(None),
        (Some(canonical), Some(version)) => {
            let definition = crate::resource::codec::decode_filesystem(canonical, version)?;
            anyhow::ensure!(
                definition.name == *name,
                "stored filesystem instance spec name does not match row name"
            );
            Ok(Some(definition.spec))
        },
        _ => anyhow::bail!("stored filesystem spec and version presence differ"),
    }
}

fn decode_optional_version(
    row: &SqliteRow,
    column: &str,
) -> anyhow::Result<Option<FilesystemVersion>> {
    row.optional_bytes(column)?
        .map(|bytes| {
            let digest: [u8; 32] = bytes.clone().try_into().map_err(|_| {
                anyhow::anyhow!(
                    "stored filesystem `{column}` has {} bytes; expected 32",
                    bytes.len()
                )
            })?;
            Ok(FilesystemVersion::from_digest(digest))
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
