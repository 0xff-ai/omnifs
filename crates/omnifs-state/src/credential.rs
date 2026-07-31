//! Credential documents, their durable rows, and credential mutations.

use anyhow::Context as _;
use omnifs_auth::{AuthKind, CredentialId};
use omnifs_core::{
    ActionId, AuthRuntimeFingerprint, CredentialGeneration, CredentialVersion, ProviderId,
};
use sqlx::FromRow;
use sqlx::sqlite::SqliteRow;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::db::Db;
use crate::row::{RowExt as _, decode_error, sql_int};
use crate::{CredentialMutationOutcome, CredentialWriteError};

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretMaterial(Vec<u8>);

impl SecretMaterial {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SecretMaterial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretMaterial([REDACTED])")
    }
}

pub struct CredentialDocument {
    pub id: CredentialId,
    pub provider: ProviderId,
    pub kind: AuthKind,
    pub auth_fingerprint: AuthRuntimeFingerprint,
    /// Effective non-secret scopes from the submitted OAuth exchange.
    /// Static-token documents carry an empty list.
    pub scopes: Vec<String>,
    pub material: SecretMaterial,
}

impl std::fmt::Debug for CredentialDocument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialDocument")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("kind", &self.kind)
            .field("auth_fingerprint", &self.auth_fingerprint)
            .field("scopes", &self.scopes)
            .field("material", &self.material)
            .finish()
    }
}

/// The reason an OAuth refresh changes durable credential state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialRefreshKind {
    /// The refresh continues the same effective grant.
    Routine,
    /// The refresh may change the effective grant and needs republication.
    AuthorityChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CredentialState {
    Active,
    Blocked,
    PendingRepublish,
    RevocationPending,
    RevocationUnknown,
    Deleted,
}

impl CredentialState {
    /// These strings are the `credentials.status` CHECK constraint in
    /// `migrations/0001_initial.sql`; keep both sides in step.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::PendingRepublish => "pending-republish",
            Self::RevocationPending => "revocation-pending",
            Self::RevocationUnknown => "revocation-unknown",
            Self::Deleted => "deleted",
        }
    }

    pub(crate) fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "blocked" => Ok(Self::Blocked),
            "pending-republish" => Ok(Self::PendingRepublish),
            "revocation-pending" => Ok(Self::RevocationPending),
            "revocation-unknown" => Ok(Self::RevocationUnknown),
            "deleted" => Ok(Self::Deleted),
            _ => anyhow::bail!("invalid credential state `{value}`"),
        }
    }
}

fn auth_kind_from_str(value: &str) -> anyhow::Result<AuthKind> {
    match value {
        "static-token" => Ok(AuthKind::StaticToken),
        "oauth" => Ok(AuthKind::OAuth),
        _ => anyhow::bail!("invalid credential kind `{value}`"),
    }
}

const fn auth_kind_as_str(kind: AuthKind) -> &'static str {
    match kind {
        AuthKind::StaticToken => "static-token",
        AuthKind::OAuth => "oauth",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialSummary {
    pub id: CredentialId,
    pub provider: ProviderId,
    pub kind: AuthKind,
    pub auth_fingerprint: AuthRuntimeFingerprint,
    pub version: CredentialVersion,
    pub generation: CredentialGeneration,
    /// Monotonic precondition for durable credential actions.
    pub action_generation: u64,
    pub state: CredentialState,
}

/// The three credential SELECT shapes. Each states its own column list once;
/// `concat!` keeps the result a `&'static str`, which is what `sqlx::query_as`
/// accepts, and the shapes genuinely differ in what they read.
macro_rules! credential_summaries_query {
    ($tail:literal) => {
        concat!(
            "SELECT provider_name, scheme, account, provider_digest, kind, \
             auth_fingerprint, version, generation, action_generation, status \
             FROM credentials ",
            $tail
        )
    };
}
pub(crate) use credential_summaries_query;

macro_rules! stored_credentials_query {
    ($tail:literal) => {
        concat!(
            "SELECT provider_name, scheme, account, provider_digest, kind, material, \
             auth_fingerprint, version, generation, action_generation, status \
             FROM credentials ",
            $tail
        )
    };
}
pub(crate) use stored_credentials_query;

impl CredentialSummary {
    fn decode_row(row: &SqliteRow) -> anyhow::Result<Self> {
        Ok(Self {
            id: CredentialId::new(
                row.text("provider_name")?,
                row.text("scheme")?,
                row.text("account")?,
            )
            .context("stored credential has invalid identity")?,
            provider: ProviderId::from_digest(row.digest("provider_digest")?),
            kind: auth_kind_from_str(&row.text("kind")?)?,
            auth_fingerprint: AuthRuntimeFingerprint::from_digest(row.digest("auth_fingerprint")?),
            version: CredentialVersion::new(row.counter("version")?),
            generation: CredentialGeneration::new(row.counter("generation")?),
            action_generation: row.unsigned("action_generation")?,
            state: CredentialState::from_str(&row.text("status")?)?,
        })
    }
}

impl FromRow<'_, SqliteRow> for CredentialSummary {
    fn from_row(row: &SqliteRow) -> sqlx::Result<Self> {
        Self::decode_row(row).map_err(decode_error)
    }
}

#[derive(Debug)]
pub struct StoredCredential {
    pub summary: CredentialSummary,
    pub material: SecretMaterial,
}

impl StoredCredential {
    fn decode_row(row: &SqliteRow) -> anyhow::Result<Self> {
        Ok(Self {
            summary: CredentialSummary::decode_row(row)?,
            material: SecretMaterial::new(row.bytes("material")?),
        })
    }
}

impl FromRow<'_, SqliteRow> for StoredCredential {
    fn from_row(row: &SqliteRow) -> sqlx::Result<Self> {
        Self::decode_row(row).map_err(decode_error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialRevocationFinish {
    Deleted,
    Unknown,
}

/// Non-secret result of an internal credential refresh or its activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRefreshOutcome {
    pub id: CredentialId,
    pub provider: ProviderId,
    pub kind: AuthKind,
    pub auth_fingerprint: AuthRuntimeFingerprint,
    pub version: CredentialVersion,
    pub generation: CredentialGeneration,
    pub state: CredentialState,
}

impl Db<'_> {
    pub(crate) async fn submit_credential_row(
        &mut self,
        document: CredentialDocument,
    ) -> Result<CredentialMutationOutcome, CredentialWriteError> {
        self.verify_credential_provider(&document).await?;
        let current = self.credential_summary(&document.id).await?;
        let (version, generation) = next_submitted(current.as_ref())?;
        let scopes = match document.kind {
            AuthKind::StaticToken => Vec::new(),
            AuthKind::OAuth => document.scopes,
        };
        sqlx::query(
            "INSERT INTO credentials(\
                 provider_name, provider_digest, scheme, account, kind, material, \
                 auth_fingerprint, version, generation, status, revocation_intent, \
                 updated_at\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', NULL, unixepoch()) \
             ON CONFLICT(provider_name, scheme, account) DO UPDATE SET \
                 provider_digest = excluded.provider_digest, kind = excluded.kind, \
                 material = excluded.material, auth_fingerprint = excluded.auth_fingerprint, \
                 version = excluded.version, generation = excluded.generation, \
                 status = excluded.status, revocation_intent = NULL, \
                 updated_at = excluded.updated_at",
        )
        .bind(document.id.provider_name())
        .bind(document.provider.as_bytes().as_slice())
        .bind(document.id.scheme())
        .bind(document.id.account())
        .bind(auth_kind_as_str(document.kind))
        .bind(document.material.expose())
        .bind(document.auth_fingerprint.as_bytes().as_slice())
        .bind(sql_int(version.get(), "credential version")?)
        .bind(sql_int(generation.get(), "credential generation")?)
        .execute(self.raw())
        .await
        .context("write credential")?;
        Ok(outcome(
            &CredentialSummary {
                id: document.id,
                provider: document.provider,
                kind: document.kind,
                auth_fingerprint: document.auth_fingerprint,
                version,
                generation,
                action_generation: current
                    .as_ref()
                    .map_or(0, |summary| summary.action_generation),
                state: CredentialState::Active,
            },
            scopes,
        ))
    }

    pub(crate) async fn begin_credential_revocation_row(
        &mut self,
        id: CredentialId,
        action_id: ActionId,
        scopes: Vec<String>,
    ) -> Result<CredentialMutationOutcome, CredentialWriteError> {
        let current = self.require_credential_exists(&id).await?;
        if current.kind != AuthKind::OAuth
            || !matches!(
                current.state,
                CredentialState::Active
                    | CredentialState::Blocked
                    | CredentialState::PendingRepublish
            )
        {
            return Err(CredentialWriteError::InvalidState {
                id,
                expected: "revocable OAuth credential",
                actual: current.state,
            });
        }
        let version = current
            .version
            .next()
            .context("credential version exhausted")?;
        let generation = current
            .generation
            .next()
            .context("credential generation exhausted")?;
        sqlx::query(
            "UPDATE credentials SET version = ?4, generation = ?5, \
             status = 'revocation-pending', revocation_intent = ?6, \
             updated_at = unixepoch() \
             WHERE provider_name = ?1 AND scheme = ?2 AND account = ?3",
        )
        .bind(id.provider_name())
        .bind(id.scheme())
        .bind(id.account())
        .bind(sql_int(version.get(), "credential version")?)
        .bind(sql_int(generation.get(), "credential generation")?)
        .bind(action_id.as_bytes().as_slice())
        .execute(self.raw())
        .await
        .context("begin credential revocation")?;
        Ok(outcome(
            &CredentialSummary {
                id,
                version,
                generation,
                state: CredentialState::RevocationPending,
                ..current
            },
            scopes,
        ))
    }

    /// Complete a revocation an out-of-band provider call finished, matching
    /// it against the durable action id recorded when revocation began.
    pub(crate) async fn write_credential_revocation_finish(
        &mut self,
        id: CredentialId,
        action_id: ActionId,
        finish_kind: CredentialRevocationFinish,
        scopes: Vec<String>,
    ) -> Result<CredentialMutationOutcome, CredentialWriteError> {
        self.transact("credential revocation", async move |db| {
            db.finish_credential_revocation_row(id, action_id, finish_kind, scopes)
                .await
        })
        .await
    }

    async fn finish_credential_revocation_row(
        &mut self,
        id: CredentialId,
        action_id: ActionId,
        finish_kind: CredentialRevocationFinish,
        scopes: Vec<String>,
    ) -> Result<CredentialMutationOutcome, CredentialWriteError> {
        let Some(current) = self.credential_summary(&id).await? else {
            return Err(CredentialWriteError::NotFound(id));
        };
        if !matches!(
            current.state,
            CredentialState::RevocationPending | CredentialState::RevocationUnknown
        ) {
            return Err(CredentialWriteError::InvalidState {
                id,
                expected: "pending or unknown revocation",
                actual: current.state,
            });
        }
        let intent: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT revocation_intent FROM credentials \
             WHERE provider_name = ?1 AND scheme = ?2 AND account = ?3",
        )
        .bind(id.provider_name())
        .bind(id.scheme())
        .bind(id.account())
        .fetch_one(self.raw())
        .await
        .context("load credential revocation intent")?;
        let intent = intent.context("credential revocation intent is missing")?;
        let stored = <[u8; 16]>::try_from(intent.as_slice())
            .map(ActionId::from_bytes)
            .context("decode credential revocation action id")?;
        if stored != action_id {
            return Err(anyhow::anyhow!(
                "credential revocation action id does not match caller-supplied action id"
            )
            .into());
        }
        let version = current
            .version
            .next()
            .context("credential version exhausted")?;
        let (state, material, retained_intent) = match finish_kind {
            CredentialRevocationFinish::Deleted => {
                (CredentialState::Deleted, Some(Vec::<u8>::new()), None)
            },
            CredentialRevocationFinish::Unknown => {
                (CredentialState::RevocationUnknown, None, Some(intent))
            },
        };
        sqlx::query(
            "UPDATE credentials SET material = COALESCE(?4, material), version = ?5, \
             status = ?6, revocation_intent = ?7, updated_at = unixepoch() \
             WHERE provider_name = ?1 AND scheme = ?2 AND account = ?3",
        )
        .bind(id.provider_name())
        .bind(id.scheme())
        .bind(id.account())
        .bind(material)
        .bind(sql_int(version.get(), "credential version")?)
        .bind(state.as_str())
        .bind(retained_intent)
        .execute(self.raw())
        .await
        .context("finish credential revocation")?;
        Ok(outcome(
            &CredentialSummary {
                id,
                version,
                state,
                ..current
            },
            match finish_kind {
                CredentialRevocationFinish::Deleted => Vec::new(),
                CredentialRevocationFinish::Unknown => scopes,
            },
        ))
    }

    pub(crate) async fn write_credential_refresh(
        &mut self,
        document: CredentialDocument,
        expected_version: CredentialVersion,
        kind: CredentialRefreshKind,
    ) -> Result<CredentialRefreshOutcome, CredentialWriteError> {
        self.transact("credential refresh", async move |db| {
            db.refresh_credential_row(document, expected_version, kind)
                .await
        })
        .await
    }

    async fn refresh_credential_row(
        &mut self,
        document: CredentialDocument,
        expected_version: CredentialVersion,
        kind: CredentialRefreshKind,
    ) -> Result<CredentialRefreshOutcome, CredentialWriteError> {
        let current = self
            .require_credential(&document.id, expected_version)
            .await?;
        if current.provider != document.provider
            || current.kind != document.kind
            || current.auth_fingerprint != document.auth_fingerprint
        {
            return Err(CredentialWriteError::FactsMismatch { id: document.id });
        }
        if current.state != CredentialState::Active {
            return Err(CredentialWriteError::InvalidState {
                id: document.id,
                expected: "active",
                actual: current.state,
            });
        }
        let version = current
            .version
            .next()
            .context("credential version exhausted")?;
        let (generation, status) = match kind {
            CredentialRefreshKind::Routine => (current.generation, CredentialState::Active),
            CredentialRefreshKind::AuthorityChanged => (
                current
                    .generation
                    .next()
                    .context("credential generation exhausted")?,
                CredentialState::PendingRepublish,
            ),
        };
        sqlx::query(
            "UPDATE credentials SET material = ?4, version = ?5, generation = ?6, \
             status = ?7, revocation_intent = NULL, updated_at = unixepoch() \
             WHERE provider_name = ?1 AND scheme = ?2 AND account = ?3",
        )
        .bind(document.id.provider_name())
        .bind(document.id.scheme())
        .bind(document.id.account())
        .bind(document.material.expose())
        .bind(sql_int(version.get(), "credential version")?)
        .bind(sql_int(generation.get(), "credential generation")?)
        .bind(status.as_str())
        .execute(self.raw())
        .await
        .context("write credential refresh")?;
        Ok(refresh_outcome(&CredentialSummary {
            id: document.id,
            provider: document.provider,
            kind: document.kind,
            auth_fingerprint: document.auth_fingerprint,
            version,
            generation,
            action_generation: current.action_generation,
            state: status,
        }))
    }

    pub(crate) async fn activate_refreshed_credential(
        &mut self,
        id: CredentialId,
        expected_version: CredentialVersion,
        expected_generation: CredentialGeneration,
    ) -> Result<CredentialRefreshOutcome, CredentialWriteError> {
        self.transact("credential activation", async move |db| {
            db.activate_refreshed_credential_row(id, expected_version, expected_generation)
                .await
        })
        .await
    }

    async fn activate_refreshed_credential_row(
        &mut self,
        id: CredentialId,
        expected_version: CredentialVersion,
        expected_generation: CredentialGeneration,
    ) -> Result<CredentialRefreshOutcome, CredentialWriteError> {
        let current = self.require_credential(&id, expected_version).await?;
        if current.generation != expected_generation {
            return Err(CredentialWriteError::GenerationConflict {
                id,
                expected: expected_generation,
                actual: current.generation,
            });
        }
        if current.state != CredentialState::PendingRepublish {
            return Err(CredentialWriteError::InvalidState {
                id,
                expected: "pending-republish",
                actual: current.state,
            });
        }
        sqlx::query(
            "UPDATE credentials SET status = 'active', updated_at = unixepoch() \
             WHERE provider_name = ?1 AND scheme = ?2 AND account = ?3",
        )
        .bind(id.provider_name())
        .bind(id.scheme())
        .bind(id.account())
        .execute(self.raw())
        .await
        .context("activate refreshed credential")?;
        Ok(refresh_outcome(&CredentialSummary {
            id,
            state: CredentialState::Active,
            ..current
        }))
    }

    async fn credential_summary(
        &mut self,
        id: &CredentialId,
    ) -> Result<Option<CredentialSummary>, CredentialWriteError> {
        Ok(
            sqlx::query_as::<_, CredentialSummary>(credential_summaries_query!(
                "WHERE provider_name = ?1 AND scheme = ?2 AND account = ?3"
            ))
            .bind(id.provider_name())
            .bind(id.scheme())
            .bind(id.account())
            .fetch_optional(self.raw())
            .await
            .context("read credential")?,
        )
    }

    /// Load one credential and prove it is at the version the caller expects.
    ///
    /// Used only by the background refresh/activation path. It keeps real
    /// compare-and-swap semantics against concurrently updated credentials.
    async fn require_credential(
        &mut self,
        id: &CredentialId,
        expected: CredentialVersion,
    ) -> Result<CredentialSummary, CredentialWriteError> {
        let Some(current) = self.credential_summary(id).await? else {
            return Err(CredentialWriteError::NotFound(id.clone()));
        };
        if current.version != expected {
            return Err(CredentialWriteError::Conflict {
                id: id.clone(),
                expected,
                actual: current.version,
            });
        }
        Ok(current)
    }

    /// Load one credential and prove it exists. The caller's durable action
    /// runs through the state writer, so a plain primary-key lookup is enough.
    async fn require_credential_exists(
        &mut self,
        id: &CredentialId,
    ) -> Result<CredentialSummary, CredentialWriteError> {
        self.credential_summary(id)
            .await?
            .ok_or_else(|| CredentialWriteError::NotFound(id.clone()))
    }

    async fn verify_credential_provider(
        &mut self,
        document: &CredentialDocument,
    ) -> Result<(), CredentialWriteError> {
        let name: Option<String> =
            sqlx::query_scalar("SELECT name FROM providers WHERE digest = ?1")
                .bind(document.provider.as_bytes().as_slice())
                .fetch_optional(self.raw())
                .await
                .context("load credential provider")?;
        let name =
            name.with_context(|| format!("provider {} is not retained", document.provider))?;
        if name != document.id.provider_name() {
            return Err(anyhow::anyhow!(
                "credential provider name does not match retained provider"
            )
            .into());
        }
        Ok(())
    }
}

/// The version and generation a credential update lands on. The durable
/// resource apply is an unconditional upsert, so only counter exhaustion can
/// error.
///
/// The durable write decides this inside its transaction, and the daemon has to
/// decide the same thing earlier to build the candidate serving generation. One
/// function so the two cannot drift.
pub fn next_submitted(
    current: Option<&CredentialSummary>,
) -> anyhow::Result<(CredentialVersion, CredentialGeneration)> {
    match current {
        None => Ok((
            CredentialVersion::initial(),
            CredentialGeneration::initial(),
        )),
        Some(current) => Ok((
            current
                .version
                .next()
                .context("credential version exhausted")?,
            current
                .generation
                .next()
                .context("credential generation exhausted")?,
        )),
    }
}

fn outcome(summary: &CredentialSummary, scopes: Vec<String>) -> CredentialMutationOutcome {
    CredentialMutationOutcome {
        provider_name: summary.id.provider_name().to_owned(),
        scheme: summary.id.scheme().to_owned(),
        account_label: summary.id.account().to_owned(),
        provider: summary.provider,
        kind: summary.kind,
        scopes,
        auth_fingerprint: summary.auth_fingerprint,
        version: summary.version,
        generation: summary.generation,
        state: summary.state,
    }
}

fn refresh_outcome(summary: &CredentialSummary) -> CredentialRefreshOutcome {
    CredentialRefreshOutcome {
        id: summary.id.clone(),
        provider: summary.provider,
        kind: summary.kind,
        auth_fingerprint: summary.auth_fingerprint,
        version: summary.version,
        generation: summary.generation,
        state: summary.state,
    }
}
