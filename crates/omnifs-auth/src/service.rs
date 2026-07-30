//! Startup-owned credential bindings and live OAuth refresh.

use crate::CredentialEntry;
use crate::client::OAuthClient;
use crate::error::AuthError;
use crate::request::OAuthRequest;
use crate::{AuthKind, CredentialId};
use arc_swap::ArcSwapOption;
use async_singleflight::Group;
use omnifs_core::CredentialVersion;
use secrecy::ExposeSecret;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use time::OffsetDateTime;

/// The single freshness margin used by authorization and health decisions.
#[allow(clippy::duration_suboptimal_units)]
pub const REFRESH_WINDOW: Duration = Duration::from_secs(60);

/// Why a bound credential could not be authorized. These errors never carry
/// credential material.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AuthUnavailable {
    #[error("no credential is stored")]
    Missing,
    #[error("credential needs re-authentication")]
    NeedsConsent,
    #[error("credential is expired")]
    Expired,
    #[error("credential refresh failed: {0}")]
    RefreshFailed(String),
    #[error("credential refresh needs namespace republication")]
    RefreshPending,
}

/// Whether a refresh keeps the same effective authorization grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshClassification {
    /// Only short-lived token material changed.
    Routine,
    /// Scopes or upstream identity changed, so the serving generation must be rebuilt.
    AuthorityChanged,
}

/// A credential snapshot used to construct a durable service.
///
/// The entry contains secret material and therefore uses the redacted
/// [`CredentialEntry`] debug implementation. The version is the exact CAS
/// value read from durable state at generation construction time.
#[derive(Clone)]
pub struct DurableCredentialSnapshot {
    pub entry: CredentialEntry,
    pub version: CredentialVersion,
}

impl std::fmt::Debug for DurableCredentialSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DurableCredentialSnapshot")
            .field("entry", &self.entry)
            .field("version", &self.version)
            .finish()
    }
}

/// The exact refresh candidate submitted to daemon-owned durable state.
pub struct RefreshCandidate {
    pub credential_id: CredentialId,
    pub expected_version: CredentialVersion,
    pub refreshed: CredentialEntry,
    pub classification: RefreshClassification,
}

impl std::fmt::Debug for RefreshCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RefreshCandidate")
            .field("credential_id", &self.credential_id)
            .field("expected_version", &self.expected_version)
            .field("refreshed", &self.refreshed)
            .field("classification", &self.classification)
            .finish()
    }
}

/// The durable result of a refresh CAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPersistence {
    /// The new token is active and may be exposed to the request.
    Active { version: CredentialVersion },
    /// The new token is durable but blocked until a new generation publishes it.
    PendingRepublish { version: CredentialVersion },
}

/// Non-secret failures from the daemon's refresh persistence boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RefreshPersistError {
    #[error("credential refresh compare-and-swap conflict")]
    Conflict,
    #[error("credential refresh persistence is unavailable")]
    Unavailable,
    #[error("credential refresh was rejected")]
    Rejected,
}

/// A narrow async boundary for durable refresh CAS operations.
pub trait RefreshSink: Send + Sync {
    fn persist<'a>(
        &'a self,
        candidate: RefreshCandidate,
    ) -> Pin<Box<dyn Future<Output = Result<RefreshPersistence, RefreshPersistError>> + Send + 'a>>;
}

/// Coarse non-secret health for one mount-owned binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialHealth {
    Ready,
    ExpiringSoon,
    Expired,
    RefreshFailed { attempts: u32 },
    NeedsConsent,
    Missing,
    StaticUnvalidated,
}

/// HTTP rejection evidence reported by the host callout path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectionEvidence {
    pub status: u16,
    pub www_authenticate: Option<String>,
}

impl RejectionEvidence {
    #[must_use]
    pub fn new(status: u16, www_authenticate: Option<String>) -> Self {
        Self {
            status,
            www_authenticate,
        }
    }

    fn asks_for_refresh(&self) -> bool {
        self.status == 401
            || (self.status == 403
                && self
                    .www_authenticate
                    .as_deref()
                    .is_some_and(Self::bearer_invalid_token))
    }

    fn bearer_invalid_token(challenges: &str) -> bool {
        let mut in_bearer = false;
        for part in challenges
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            if let Some((scheme, params)) = strip_auth_scheme(part) {
                in_bearer = scheme.eq_ignore_ascii_case("bearer");
                if in_bearer && auth_param_is_invalid_token(params) {
                    return true;
                }
                continue;
            }
            if in_bearer && auth_param_is_invalid_token(part) {
                return true;
            }
        }
        false
    }
}

/// Result of handling an upstream credential rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    Refreshed,
    NoCredential,
    NotApplicable,
    RefreshFailed(String),
}

#[derive(Debug, Clone)]
struct RefreshFailure {
    needs_consent: bool,
    pending: bool,
    message: String,
}

struct DurableBackend {
    snapshots: RwLock<HashMap<CredentialId, DurableCredentialSnapshot>>,
    pending: RwLock<HashSet<CredentialId>>,
    sink: Arc<dyn RefreshSink>,
}

/// A single immutable mount binding. Its injection facts belong to the mount,
/// while the shared service owns durable storage and refresh single-flight.
pub struct AuthBinding {
    service: Arc<CredentialService>,
    id: CredentialId,
    kind: AuthKind,
    domains: Vec<String>,
    header_name: String,
    value_prefix: String,
    request: Option<OAuthRequest>,
    current: ArcSwapOption<CredentialEntry>,
    refresh_failures: AtomicU32,
    needs_consent: AtomicBool,
}

impl AuthBinding {
    fn new(
        service: Arc<CredentialService>,
        id: CredentialId,
        domains: Vec<String>,
        header_name: String,
        value_prefix: String,
        request: Option<OAuthRequest>,
        current: Option<CredentialEntry>,
    ) -> Self {
        let kind = if request.is_some() {
            AuthKind::OAuth
        } else {
            AuthKind::StaticToken
        };
        Self {
            service,
            id,
            kind,
            domains,
            header_name,
            value_prefix,
            request,
            current: ArcSwapOption::new(current.map(Arc::new)),
            refresh_failures: AtomicU32::new(0),
            needs_consent: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn credential_id(&self) -> &CredentialId {
        &self.id
    }

    #[must_use]
    pub fn applies_to_url(&self, url: &str) -> bool {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(String::from));
        host.is_some_and(|host| self.domains.iter().any(|domain| domain == &host))
    }

    /// Compare shared runtime identity. Injection facts are deliberately
    /// excluded because they belong to each consuming mount.
    #[must_use]
    pub fn same_runtime_as(&self, other: &Self) -> bool {
        self.id == other.id
            && self.kind == other.kind
            && match (&self.request, &other.request) {
                (Some(left), Some(right)) => left.has_same_runtime_metadata(right),
                (None, None) => true,
                _ => false,
            }
    }

    #[must_use]
    pub fn health(&self) -> CredentialHealth {
        let Some(entry) = self.current.load_full() else {
            return CredentialHealth::Missing;
        };
        if self.needs_consent.load(Ordering::Relaxed) {
            return CredentialHealth::NeedsConsent;
        }
        if self.kind == AuthKind::StaticToken {
            return CredentialHealth::StaticUnvalidated;
        }
        let now = OffsetDateTime::now_utc();
        let failures = self.refresh_failures.load(Ordering::Relaxed);
        if failures > 0 && !CredentialService::is_fresh(&entry, now) {
            return CredentialHealth::RefreshFailed { attempts: failures };
        }
        if entry.is_expired_at(now) {
            return if entry.refresh_token().is_some() {
                CredentialHealth::Expired
            } else {
                CredentialHealth::NeedsConsent
            };
        }
        if CredentialService::is_fresh(&entry, now) {
            CredentialHealth::Ready
        } else {
            CredentialHealth::ExpiringSoon
        }
    }

    /// Resolve the final header tuple for a URL. The secret is exposed only
    /// while composing this tuple at the existing HTTP wire boundary.
    pub async fn authorization_for(
        &self,
        url: &str,
    ) -> Result<Option<(String, String)>, AuthUnavailable> {
        if !self.applies_to_url(url) {
            return Ok(None);
        }
        if self.needs_consent.load(Ordering::Relaxed) {
            return Err(AuthUnavailable::NeedsConsent);
        }
        if self.service.refresh_pending(&self.id)? {
            return Err(AuthUnavailable::RefreshPending);
        }
        let Some(entry) = self.current.load_full() else {
            return Err(AuthUnavailable::Missing);
        };
        let entry = if self.kind == AuthKind::StaticToken
            || CredentialService::is_fresh(&entry, OffsetDateTime::now_utc())
        {
            entry
        } else {
            match self.refresh(false).await {
                Ok(Some(entry)) => Arc::new(entry),
                Ok(None) => return Err(AuthUnavailable::Missing),
                Err(error) => return Err(error),
            }
        };
        Ok(Some(self.header_for(&entry)))
    }

    pub async fn report_rejected_for_response(
        &self,
        url: &str,
        status: u16,
        www_authenticate: Option<String>,
    ) -> RefreshOutcome {
        if !self.applies_to_url(url) {
            return RefreshOutcome::NotApplicable;
        }
        let evidence = RejectionEvidence::new(status, www_authenticate);
        if !evidence.asks_for_refresh() || self.kind != AuthKind::OAuth {
            return RefreshOutcome::NotApplicable;
        }
        if self.needs_consent.load(Ordering::Relaxed) {
            return RefreshOutcome::RefreshFailed(AuthUnavailable::NeedsConsent.to_string());
        }
        match self.service.refresh_pending(&self.id) {
            Ok(true) => {
                return RefreshOutcome::RefreshFailed(AuthUnavailable::RefreshPending.to_string());
            },
            Ok(false) => {},
            Err(error) => return RefreshOutcome::RefreshFailed(error.to_string()),
        }
        match self.refresh(true).await {
            Ok(Some(_)) => RefreshOutcome::Refreshed,
            Ok(None) | Err(AuthUnavailable::Missing) => RefreshOutcome::NoCredential,
            Err(error) => RefreshOutcome::RefreshFailed(error.to_string()),
        }
    }

    async fn refresh(&self, force: bool) -> Result<Option<CredentialEntry>, AuthUnavailable> {
        let Some(request) = self.request.clone() else {
            return Ok(None);
        };
        let observed = self.current.load_full();
        match self
            .service
            .refresh(&self.id, &request, observed.as_deref(), force)
            .await
        {
            Ok(Some(entry)) => {
                self.current.store(Some(Arc::new(entry.clone())));
                self.refresh_failures.store(0, Ordering::Relaxed);
                self.needs_consent.store(false, Ordering::Relaxed);
                Ok(Some(entry))
            },
            Ok(None) => {
                self.current.store(None);
                Ok(None)
            },
            Err(error) => {
                if error.needs_consent {
                    self.needs_consent.store(true, Ordering::Relaxed);
                }
                if error.pending {
                    return Err(AuthUnavailable::RefreshPending);
                }
                let attempts = self.refresh_failures.fetch_add(1, Ordering::Relaxed) + 1;
                Err(if error.needs_consent {
                    AuthUnavailable::NeedsConsent
                } else {
                    AuthUnavailable::RefreshFailed(format!(
                        "{} (attempt {attempts})",
                        error.message
                    ))
                })
            },
        }
    }

    fn header_for(&self, entry: &CredentialEntry) -> (String, String) {
        (
            self.header_name.clone(),
            format!(
                "{}{}",
                self.value_prefix,
                entry.access_token().expose_secret()
            ),
        )
    }

    #[cfg(test)]
    pub(crate) fn current_for_test(&self) -> Option<CredentialEntry> {
        self.current.load_full().map(|entry| (*entry).clone())
    }
}

/// Shared durable store and OAuth transport. Mount bindings retain the loaded
/// entry and runtime metadata; this service has no mount registry.
pub struct CredentialService {
    backend: Arc<DurableBackend>,
    oauth: OAuthClient,
    refreshes: Group<String, Option<CredentialEntry>, RefreshFailure>,
}

impl CredentialService {
    /// Creates a service backed by daemon-owned durable refresh CAS.
    ///
    /// The snapshots must come from one serving generation and include the
    /// exact durable version read with each entry. The sink is the sole
    /// persistence authority and is awaited before a routine token becomes
    /// visible.
    #[must_use]
    pub fn new(
        snapshots: impl IntoIterator<Item = (CredentialId, DurableCredentialSnapshot)>,
        oauth: OAuthClient,
        sink: Arc<dyn RefreshSink>,
    ) -> Self {
        Self {
            backend: Arc::new(DurableBackend {
                snapshots: RwLock::new(snapshots.into_iter().collect()),
                pending: RwLock::new(HashSet::new()),
                sink,
            }),
            oauth,
            refreshes: Group::new(),
        }
    }

    fn is_fresh(entry: &CredentialEntry, now: OffsetDateTime) -> bool {
        let window = time::Duration::try_from(REFRESH_WINDOW)
            .expect("REFRESH_WINDOW fits in time::Duration");
        entry
            .expires_at()
            .is_none_or(|expires_at| expires_at - now > window)
    }

    pub fn bind_static(
        self: &Arc<Self>,
        id: CredentialId,
        domains: Vec<String>,
        header_name: String,
        value_prefix: String,
    ) -> Result<AuthBinding, AuthError> {
        let current = self.current_entry(&id)?;
        validate_kind(&id, AuthKind::StaticToken, current.as_ref())?;
        Ok(AuthBinding::new(
            Arc::clone(self),
            id,
            domains,
            header_name,
            value_prefix,
            None,
            current,
        ))
    }

    pub fn bind_oauth(
        self: &Arc<Self>,
        id: CredentialId,
        request: OAuthRequest,
        domains: Vec<String>,
        header_name: String,
        value_prefix: String,
    ) -> Result<AuthBinding, AuthError> {
        let current = self.current_entry(&id)?;
        validate_kind(&id, AuthKind::OAuth, current.as_ref())?;
        Ok(AuthBinding::new(
            Arc::clone(self),
            id,
            domains,
            header_name,
            value_prefix,
            Some(request),
            current,
        ))
    }

    async fn refresh(
        &self,
        id: &CredentialId,
        request: &OAuthRequest,
        observed: Option<&CredentialEntry>,
        force: bool,
    ) -> Result<Option<CredentialEntry>, RefreshFailure> {
        let key = id.storage_key();
        let request = request.clone();
        let observed = observed.cloned();
        match self
            .refreshes
            .work(&key, async move {
                self.do_refresh(id, &request, observed.as_ref(), force)
                    .await
            })
            .await
        {
            Ok(result) => Ok(result),
            Err(Some(error)) => Err(error),
            Err(None) => Err(RefreshFailure {
                needs_consent: false,
                pending: false,
                message: "refresh leader failed".to_string(),
            }),
        }
    }

    async fn do_refresh(
        &self,
        id: &CredentialId,
        request: &OAuthRequest,
        observed: Option<&CredentialEntry>,
        force: bool,
    ) -> Result<Option<CredentialEntry>, RefreshFailure> {
        let snapshot = self.backend.snapshot(id).map_err(|()| RefreshFailure {
            needs_consent: false,
            pending: false,
            message: "credential snapshot unavailable".to_owned(),
        })?;
        let expected_version = snapshot.as_ref().map(|snapshot| snapshot.version);
        let stored = snapshot.map(|snapshot| snapshot.entry);
        let stored = stored.filter(|entry| entry.kind() == AuthKind::OAuth);
        let Some(stored) = stored else {
            return Ok(None);
        };
        if Self::stored_satisfies(&stored, observed, OffsetDateTime::now_utc(), force) {
            return Ok(Some(stored));
        }
        if stored.refresh_token().is_none() {
            return Err(RefreshFailure {
                needs_consent: true,
                pending: false,
                message: format!("OAuth credential {id} requires re-authentication"),
            });
        }
        match self.oauth.refresh(request.clone(), &stored).await {
            Ok(refreshed) => {
                let expected_version = expected_version.ok_or_else(|| RefreshFailure {
                    needs_consent: false,
                    pending: false,
                    message: "credential snapshot unavailable".to_owned(),
                })?;
                persist_durable_refresh(&self.backend, id, expected_version, &stored, refreshed)
                    .await
            },
            Err(AuthError::TokenEndpoint { error, .. }) if error == "invalid_grant" => {
                Err(RefreshFailure {
                    needs_consent: true,
                    pending: false,
                    message: "OAuth refresh token was rejected".to_string(),
                })
            },
            Err(error) => Err(RefreshFailure {
                needs_consent: false,
                pending: false,
                message: error.to_string(),
            }),
        }
    }

    fn current_entry(&self, id: &CredentialId) -> Result<Option<CredentialEntry>, AuthError> {
        self.backend
            .snapshot(id)
            .map_err(|()| AuthError::RequestConfig("credential snapshot unavailable".to_owned()))
            .map(|snapshot| snapshot.map(|snapshot| snapshot.entry))
    }

    fn refresh_pending(&self, id: &CredentialId) -> Result<bool, AuthUnavailable> {
        self.backend.is_pending(id).map_err(|()| {
            AuthUnavailable::RefreshFailed("credential snapshot unavailable".to_owned())
        })
    }

    #[cfg(test)]
    pub(crate) fn version_for_test(&self, id: &CredentialId) -> Option<CredentialVersion> {
        self.backend
            .snapshot(id)
            .ok()
            .flatten()
            .map(|snapshot| snapshot.version)
    }

    fn stored_satisfies(
        stored: &CredentialEntry,
        observed: Option<&CredentialEntry>,
        now: OffsetDateTime,
        force: bool,
    ) -> bool {
        Self::is_fresh(stored, now)
            && (!force || observed.is_none_or(|entry| !same_oauth_token(stored, entry)))
    }
}

impl DurableBackend {
    fn update(
        &self,
        id: &CredentialId,
        entry: CredentialEntry,
        version: CredentialVersion,
    ) -> Result<(), ()> {
        let mut snapshots = self.snapshots.write().map_err(|_| ())?;
        snapshots.insert(id.clone(), DurableCredentialSnapshot { entry, version });
        Ok(())
    }

    fn snapshot(&self, id: &CredentialId) -> Result<Option<DurableCredentialSnapshot>, ()> {
        self.snapshots
            .read()
            .map(|snapshots| snapshots.get(id).cloned())
            .map_err(|_| ())
    }

    fn mark_pending(&self, id: &CredentialId) -> Result<(), ()> {
        self.pending.write().map_err(|_| ())?.insert(id.clone());
        Ok(())
    }

    fn is_pending(&self, id: &CredentialId) -> Result<bool, ()> {
        self.pending
            .read()
            .map(|pending| pending.contains(id))
            .map_err(|_| ())
    }
}

async fn persist_durable_refresh(
    durable: &DurableBackend,
    id: &CredentialId,
    expected_version: CredentialVersion,
    current: &CredentialEntry,
    refreshed: CredentialEntry,
) -> Result<Option<CredentialEntry>, RefreshFailure> {
    let classification = classify_refresh(current, &refreshed);
    let Some(next_version) = expected_version.next() else {
        return Err(RefreshFailure {
            needs_consent: false,
            pending: false,
            message: "credential version exhausted".to_owned(),
        });
    };
    let candidate = RefreshCandidate {
        credential_id: id.clone(),
        expected_version,
        refreshed: refreshed.clone(),
        classification,
    };
    let persisted = durable
        .sink
        .persist(candidate)
        .await
        .map_err(|error| RefreshFailure {
            needs_consent: false,
            pending: false,
            message: error.to_string(),
        })?;
    match (classification, persisted) {
        (RefreshClassification::Routine, RefreshPersistence::Active { version })
            if version == next_version =>
        {
            durable
                .update(id, refreshed.clone(), version)
                .map_err(|()| RefreshFailure {
                    needs_consent: false,
                    pending: false,
                    message: "credential snapshot unavailable".to_owned(),
                })?;
            Ok(Some(refreshed))
        },
        (
            RefreshClassification::AuthorityChanged,
            RefreshPersistence::PendingRepublish { version },
        ) if version == next_version => {
            durable.mark_pending(id).map_err(|()| RefreshFailure {
                needs_consent: false,
                pending: false,
                message: "credential snapshot unavailable".to_owned(),
            })?;
            Err(RefreshFailure {
                needs_consent: false,
                pending: true,
                message: "credential refresh needs republication".to_owned(),
            })
        },
        (
            RefreshClassification::Routine,
            RefreshPersistence::PendingRepublish { .. } | RefreshPersistence::Active { .. },
        )
        | (
            RefreshClassification::AuthorityChanged,
            RefreshPersistence::Active { .. } | RefreshPersistence::PendingRepublish { .. },
        ) => Err(RefreshFailure {
            needs_consent: false,
            pending: false,
            message: "refresh persistence returned an invalid state".to_owned(),
        }),
    }
}

fn classify_refresh(
    current: &CredentialEntry,
    refreshed: &CredentialEntry,
) -> RefreshClassification {
    let current_scopes: BTreeSet<&str> = current.scopes().iter().map(String::as_str).collect();
    let refreshed_scopes: BTreeSet<&str> = refreshed.scopes().iter().map(String::as_str).collect();
    if current_scopes == refreshed_scopes
        && current.upstream_identity() == refreshed.upstream_identity()
    {
        RefreshClassification::Routine
    } else {
        RefreshClassification::AuthorityChanged
    }
}

fn validate_kind(
    id: &CredentialId,
    expected: AuthKind,
    entry: Option<&CredentialEntry>,
) -> Result<(), AuthError> {
    if let Some(entry) = entry
        && entry.kind() != expected
    {
        return Err(AuthError::CredentialKindMismatch {
            id: id.clone(),
            expected,
            found: entry.kind(),
        });
    }
    Ok(())
}

fn same_oauth_token(left: &CredentialEntry, right: &CredentialEntry) -> bool {
    if left.access_token().expose_secret() != right.access_token().expose_secret() {
        return false;
    }
    match (left.refresh_token(), right.refresh_token()) {
        (Some(left), Some(right)) => left.expose_secret() == right.expose_secret(),
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

fn strip_auth_scheme(part: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = part.split_once(char::is_whitespace)?;
    (!scheme.contains('=')).then_some((scheme, rest.trim()))
}

fn auth_param_is_invalid_token(param: &str) -> bool {
    let Some((name, value)) = param.split_once('=') else {
        return false;
    };
    name.trim().eq_ignore_ascii_case("error")
        && value
            .trim()
            .trim_matches('"')
            .eq_ignore_ascii_case("invalid_token")
}
