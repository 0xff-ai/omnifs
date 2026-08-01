//! Connection configuration, integrity, and the shared transaction dance.

use anyhow::Context as _;
use omnifs_core::ResourceRevision;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqliteSynchronous};
use sqlx::{Connection as _, SqlitePool};
use std::fmt::Display;
use std::num::NonZeroU16;
use std::path::Path;
use std::time::Duration;

use crate::row::sql_int;

/// The durable write handle, borrowed for the length of one call.
///
/// Every statement this crate issues hangs off this type, so a bare connection
/// and a connection inside a transaction are the same thing to the code that
/// writes rows. [`Db::transact`] is the only place a `Transaction` is named.
pub(crate) struct Db<'c>(&'c mut SqliteConnection);

impl<'c> Db<'c> {
    pub(crate) const fn new(connection: &'c mut SqliteConnection) -> Self {
        Self(connection)
    }
}

impl Db<'_> {
    /// The raw connection, for the few callers that reach past SQL.
    pub(crate) fn raw(&mut self) -> &mut SqliteConnection {
        self.0
    }

    /// Run `body` inside one transaction: commit it on success, roll it back on
    /// failure, and report a rollback that itself fails rather than losing it
    /// behind the original error.
    ///
    /// Always `BEGIN IMMEDIATE`. Every mutation here reads before it writes, so
    /// a deferred transaction would have to upgrade its lock mid-way and could
    /// fail with `SQLITE_BUSY` the moment a second writer exists. Taking the
    /// write lock up front costs nothing against the one writer connection, and
    /// WAL readers are never blocked by it.
    pub(crate) async fn transact<T, E>(
        &mut self,
        what: &str,
        body: impl AsyncFnOnce(&mut Db<'_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<anyhow::Error> + Display,
    {
        let mut transaction = self
            .0
            .begin_with("BEGIN IMMEDIATE")
            .await
            .with_context(|| format!("begin {what} transaction"))?;
        let result = body(&mut Db::new(&mut transaction)).await;
        match result {
            Ok(value) => {
                transaction
                    .commit()
                    .await
                    .with_context(|| format!("commit {what} transaction"))?;
                Ok(value)
            },
            Err(error) => {
                if let Err(rollback) = transaction.rollback().await {
                    return Err(anyhow::anyhow!(
                        "{error}; {what} rollback also failed: {rollback}"
                    )
                    .into());
                }
                Err(error)
            },
        }
    }

    pub(crate) async fn request_truncating_checkpoint(&mut self) {
        match sqlx::query_scalar::<_, i64>("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&mut *self.0)
            .await
        {
            Ok(busy) if busy != 0 => {
                tracing::warn!("large provider committed; WAL checkpoint is busy");
            },
            Ok(_) => {},
            Err(error) => {
                tracing::warn!(%error, "large provider committed; WAL checkpoint failed");
            },
        }
    }

    pub(crate) async fn write_attach_port(&mut self, port: NonZeroU16) -> anyhow::Result<()> {
        let inserted = sqlx::query(
            "INSERT INTO attach_endpoint(singleton, tcp_port) VALUES (1, ?1) \
             ON CONFLICT(singleton) DO NOTHING",
        )
        .bind(i64::from(port.get()))
        .execute(&mut *self.0)
        .await
        .context("persist attach port")?
        .rows_affected();
        if inserted == 1 {
            return Ok(());
        }
        let existing = sqlx::query_scalar::<_, i64>(
            "SELECT tcp_port FROM attach_endpoint WHERE singleton = 1",
        )
        .fetch_one(&mut *self.0)
        .await
        .context("read concurrently persisted attach port")?;
        anyhow::ensure!(
            decode_attach_port(existing)? == port,
            "attach port was already persisted with a different value"
        );
        Ok(())
    }

    pub(crate) async fn write_recovery_transition(
        &mut self,
        transition: RecoveryTransition,
    ) -> anyhow::Result<()> {
        self.transact("recovery state", async move |db| {
            db.apply_recovery_transition(transition).await
        })
        .await
    }

    async fn apply_recovery_transition(
        &mut self,
        transition: RecoveryTransition,
    ) -> anyhow::Result<()> {
        match transition {
            RecoveryTransition::Serving { revision } => {
                sqlx::query(
                    "UPDATE recovery_state \
                     SET state = 'ready', detail = NULL, serving_resource_revision = ?1, \
                         updated_at = unixepoch() \
                     WHERE singleton = 1 AND serving_resource_revision <= ?1",
                )
                .bind(sql_int(revision.get(), "mount revision")?)
                .execute(&mut *self.0)
                .await
                .context("mark serving state ready")?;
            },
            RecoveryTransition::RecoveryRequired { detail } => {
                sqlx::query(
                    "UPDATE recovery_state \
                     SET state = 'recovery-required', detail = ?1, updated_at = unixepoch() \
                     WHERE singleton = 1",
                )
                .bind(detail)
                .execute(&mut *self.0)
                .await
                .context("mark recovery required")?;
            },
        }
        Ok(())
    }
}

pub(crate) fn connect_options(path: &Path, busy_timeout: Duration) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .foreign_keys(true)
        .busy_timeout(busy_timeout)
        .pragma("wal_autocheckpoint", "1000")
}

pub(crate) async fn check_integrity(pool: &SqlitePool) -> anyhow::Result<()> {
    let result: String = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_one(pool)
        .await
        .context("run StateStore integrity check")?;
    anyhow::ensure!(
        result == "ok",
        "StateStore integrity check failed: {result}"
    );
    Ok(())
}

pub(crate) async fn read_attach_port(pool: &SqlitePool) -> anyhow::Result<Option<NonZeroU16>> {
    let port =
        sqlx::query_scalar::<_, i64>("SELECT tcp_port FROM attach_endpoint WHERE singleton = 1")
            .fetch_optional(pool)
            .await
            .context("read persisted attach port")?;
    port.map(decode_attach_port).transpose()
}

fn decode_attach_port(port: i64) -> anyhow::Result<NonZeroU16> {
    let port = u16::try_from(port).context("persisted attach port exceeds u16")?;
    NonZeroU16::new(port).context("persisted attach port is zero")
}

/// One durable move of the serving head.
pub(crate) enum RecoveryTransition {
    Serving { revision: ResourceRevision },
    RecoveryRequired { detail: String },
}
