//! Proactive, fail-closed credential ownership.
//!
//! [`CredentialService`] is the single host-side owner of credential store
//! access, the one expiry predicate, and OAuth refresh. A mount's
//! injector ([`crate`] consumers in `omnifs-engine`) binds each credential
//! it needs, then asks for [`CredentialService::authorization`] per request. The
//! service reads the store, refreshes synchronously inside the refresh window,
//! coalesces concurrent refreshes per credential, and NEVER returns a stale or
//! absent credential as success. Token material remains a [`SecretString`] until
//! the consuming engine binding composes the final wire header; it is never
//! logged and never placed on a status or wire type.

use crate::client::{OAuthClient, OAuthRevokeOutcome};
use crate::error::AuthError;
use crate::request::OAuthRequest;
use arc_swap::ArcSwapOption;
use async_singleflight::Group;
use dashmap::DashMap;
use omnifs_workspace::authn::{AuthKind, CredentialId};
use omnifs_workspace::creds::{CredStoreError, CredentialEntry, CredentialStore};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::Notify;
use tracing::warn;

/// The single freshness margin. A credential is usable only when at least this
/// long remains before expiry; inside the window it is refreshed (OAuth) or
/// treated as unavailable. This is the one place the margin lives: the OAuth
/// mint path stamps the real `expires_at` with no baked-in skew, and the margin
/// is applied here, at check time.
// Expressed in seconds deliberately: this is the "60-second refresh window",
// not "1 minute" of wall-clock policy.
#[allow(clippy::duration_suboptimal_units)]
pub const REFRESH_WINDOW: Duration = Duration::from_secs(60);

/// Extra sleep the proactive refresh loop adds atop the computed wait, as a
/// fraction of it. Spreads out credentials whose deadlines coincide instead
/// of refreshing them in lockstep.
const REFRESH_JITTER_FRACTION: f64 = 0.10;

/// Floor under the refresh loop's computed sleep. A transient refresh
/// failure does not move the credential's `expires_at`, so without this
/// floor a persistent failure would recompute a due-now sleep on every
/// iteration and hammer the OAuth endpoint in a hot loop.
const REFRESH_LOOP_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Fixed seed for the refresh loop's jitter PRNG. Deterministic (not system
/// randomness) so a test driving the loop with paused time sees a
/// reproducible sequence of wake-ups.
const REFRESH_LOOP_SEED: u64 = 0x5EED_1E55_0A57_0000;

/// Why a credential could not be authorized. Every variant is a fail-closed
/// denial; none carries token material.
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
}

/// Non-secret health of one bound credential. NEVER holds token material.
#[derive(Debug, Clone)]
pub struct CredentialStatus {
    pub id: CredentialId,
    pub health: CredentialHealth,
    pub expires_at: Option<OffsetDateTime>,
    pub scopes: Vec<String>,
}

/// Coarse health classification for status/UX. Carries no secret.
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

impl CredentialHealth {
    #[must_use]
    pub fn severity(&self) -> u8 {
        match self {
            Self::Ready => 0,
            Self::StaticUnvalidated => 1,
            Self::ExpiringSoon => 2,
            Self::RefreshFailed { .. } => 3,
            Self::Expired => 4,
            Self::NeedsConsent => 5,
            Self::Missing => 6,
        }
    }
}

/// HTTP rejection evidence reported by the host callout path.
///
/// Classification lives here, next to refresh policy: a 401 always asks an
/// OAuth credential to rotate, while a 403 only does so when the upstream's
/// `WWW-Authenticate` challenge carries a bearer `error="invalid_token"`
/// parameter.
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

/// Result of revoking, when possible, then deleting a local credential entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeOutcome {
    Revoked,
    Unsupported,
    Failed { error: String },
    LocalOnly,
    DeleteFailed { error: String },
}

impl RevokeOutcome {
    #[must_use]
    pub fn delete_error(&self) -> Option<&str> {
        match self {
            Self::DeleteFailed { error } => Some(error.as_str()),
            Self::Revoked | Self::Unsupported | Self::Failed { .. } | Self::LocalOnly => None,
        }
    }
}

impl std::fmt::Display for RevokeOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Revoked => f.write_str("credential revoked upstream and deleted"),
            Self::Unsupported => {
                f.write_str("provider does not support revocation; deleted locally")
            },
            Self::Failed { error } => {
                write!(f, "upstream revocation failed ({error}); deleted locally")
            },
            Self::LocalOnly => f.write_str("credential deleted locally"),
            Self::DeleteFailed { error } => write!(f, "credential delete failed: {error}"),
        }
    }
}

/// Per-credential state: how to refresh it (OAuth), and the last-known entry
/// cached in memory. `current` is the token the
/// injector last resolved; `do_refresh` compares against it to decide whether a
/// forced refresh must hit the endpoint or can adopt a newer stored entry.
struct CredentialState {
    kind: AuthKind,
    /// `Some` for OAuth (carries endpoints, client credentials, refresh
    /// params); `None` for static tokens, which never refresh.
    request: Option<OAuthRequest>,
    current: ArcSwapOption<CredentialEntry>,
    refresh_failures: AtomicU32,
    needs_consent: AtomicBool,
}

impl CredentialState {
    fn authorization(&self, entry: &CredentialEntry) -> SecretString {
        entry.access_token().clone()
    }

    fn health(
        &self,
        entry: &CredentialEntry,
        now: OffsetDateTime,
        failures: u32,
    ) -> CredentialHealth {
        if self.needs_consent.load(Ordering::Relaxed) {
            return CredentialHealth::NeedsConsent;
        }
        if self.kind == AuthKind::StaticToken {
            return CredentialHealth::StaticUnvalidated;
        }
        if failures > 0 && !CredentialService::is_fresh(entry, now) {
            return CredentialHealth::RefreshFailed { attempts: failures };
        }
        if entry.is_expired_at(now) {
            return if entry.refresh_token().is_some() {
                CredentialHealth::Expired
            } else {
                CredentialHealth::NeedsConsent
            };
        }
        if CredentialService::is_fresh(entry, now) {
            CredentialHealth::Ready
        } else {
            CredentialHealth::ExpiringSoon
        }
    }
}

/// Proactive owner of credential store access, expiry, and OAuth refresh.
///
/// One instance is shared across every mount in a host: mounts that resolve to
/// the same [`CredentialId`] share one refresh state, so a single refresh serves
/// them all. Bindings are established during startup and conflicting OAuth
/// runtime metadata fails.
pub struct CredentialService {
    store: Arc<dyn CredentialStore>,
    oauth: OAuthClient,
    states: DashMap<CredentialId, Arc<CredentialState>>,
    /// Per-credential single-flight: concurrent refreshes for one id coalesce
    /// onto one endpoint call.
    refreshes: Group<String, Option<CredentialEntry>, String>,
    /// Wakes [`spawn_refresh_loop`](Self::spawn_refresh_loop)'s sleep
    /// immediately when credential state changes under it (`bind_oauth`,
    /// `store_entry`, `reload`), instead of waiting out whatever deadline it
    /// last computed.
    refresh_notify: Notify,
}

impl CredentialService {
    #[must_use]
    pub fn new(store: Arc<dyn CredentialStore>, oauth: OAuthClient) -> Self {
        Self {
            store,
            oauth,
            states: DashMap::new(),
            refreshes: Group::new(),
            refresh_notify: Notify::new(),
        }
    }

    /// The one expiry predicate. Every credential expiry decision (emit,
    /// refresh, health) routes through here. A credential with no expiry is
    /// always fresh.
    fn is_fresh(entry: &CredentialEntry, now: OffsetDateTime) -> bool {
        let window = time::Duration::try_from(REFRESH_WINDOW)
            .expect("REFRESH_WINDOW fits in time::Duration");
        entry
            .expires_at()
            .is_none_or(|expires_at| expires_at - now > window)
    }

    /// Bind a static-token credential. Store errors and stored kind mismatches
    /// fail before the binding is published.
    pub fn bind_static(&self, id: CredentialId) -> Result<(), AuthError> {
        self.bind(id, AuthKind::StaticToken, None)
    }

    /// Bind an OAuth credential. Injection facts remain in the engine binding;
    /// this request supplies only the runtime refresh metadata owned here.
    pub fn bind_oauth(&self, id: CredentialId, request: OAuthRequest) -> Result<(), AuthError> {
        self.bind(id, AuthKind::OAuth, Some(request))?;
        // A newly-bound OAuth credential can change the refresh loop's
        // nearest deadline (or give it its first one); wake it so a mount
        // added long after the loop started sleeping is not stranded until
        // some other credential's deadline happens to fire first.
        self.refresh_notify.notify_one();
        Ok(())
    }

    fn bind(
        &self,
        id: CredentialId,
        kind: AuthKind,
        request: Option<OAuthRequest>,
    ) -> Result<(), AuthError> {
        use dashmap::mapref::entry::Entry;
        match self.states.entry(id.clone()) {
            Entry::Occupied(existing) => {
                let state = existing.get();
                let same = state.kind == kind
                    && match (&state.request, &request) {
                        (Some(existing), Some(candidate)) => {
                            existing.has_same_runtime_metadata(candidate)
                        },
                        (None, None) => true,
                        _ => false,
                    };
                if same {
                    Ok(())
                } else {
                    Err(AuthError::CredentialBindingConflict { id })
                }
            },
            Entry::Vacant(slot) => {
                let stored = self.store.get(slot.key())?;
                if let Some(entry) = &stored
                    && entry.kind() != kind
                {
                    return Err(AuthError::CredentialKindMismatch {
                        id,
                        expected: kind,
                        found: entry.kind(),
                    });
                }
                slot.insert(Arc::new(CredentialState {
                    kind,
                    request,
                    current: ArcSwapOption::new(stored.map(Arc::new)),
                    refresh_failures: AtomicU32::new(0),
                    needs_consent: AtomicBool::new(false),
                }));
                Ok(())
            },
        }
    }

    /// Resolve a usable auth header for `id`, refreshing synchronously when the
    /// credential is inside the refresh window. Fails closed: a missing, expired,
    /// or unrefreshable credential returns an error, never a stale header.
    pub async fn authorization(&self, id: &CredentialId) -> Result<SecretString, AuthUnavailable> {
        let Some(state) = self.state(id) else {
            return Err(AuthUnavailable::Missing);
        };
        if state.needs_consent.load(Ordering::Relaxed) {
            return Err(AuthUnavailable::NeedsConsent);
        }
        let Some(entry) = self
            .current_entry(&state, id)
            .map_err(|error| AuthUnavailable::RefreshFailed(error.to_string()))?
        else {
            return Err(AuthUnavailable::Missing);
        };

        if state.kind == AuthKind::StaticToken {
            return Ok(state.authorization(&entry));
        }

        if Self::is_fresh(&entry, OffsetDateTime::now_utc()) {
            return Ok(state.authorization(&entry));
        }

        match self.refresh_locked(&state, id, false).await {
            Ok(Some(fresh)) => Ok(state.authorization(&fresh)),
            Ok(None) => Err(AuthUnavailable::Missing),
            Err(reason) => {
                state.refresh_failures.fetch_add(1, Ordering::Relaxed);
                Err(AuthUnavailable::RefreshFailed(reason))
            },
        }
    }

    /// The synchronous, non-refreshing counterpart of [`authorization`]: the
    /// best header available from cache-or-store without awaiting a refresh.
    /// Returns `None` for a missing or non-fresh credential.
    ///
    /// [`authorization`]: CredentialService::authorization
    #[must_use]
    pub fn cached_authorization(&self, id: &CredentialId) -> Option<SecretString> {
        let state = self.state(id)?;
        if state.needs_consent.load(Ordering::Relaxed) {
            return None;
        }
        let entry = self.current_entry(&state, id).ok().flatten()?;
        Self::is_fresh(&entry, OffsetDateTime::now_utc()).then(|| state.authorization(&entry))
    }

    /// Force a refresh of `id` (an upstream rejected the current token).
    /// `Ok(Some)` rotated the token, `Ok(None)` means no credential is stored,
    /// `Err` means the refresh failed (fail closed).
    pub async fn refresh(
        &self,
        id: &CredentialId,
    ) -> Result<Option<SecretString>, AuthUnavailable> {
        let Some(state) = self.state(id) else {
            return Ok(None);
        };
        if state.kind != AuthKind::OAuth {
            return Ok(None);
        }
        if state.needs_consent.load(Ordering::Relaxed) {
            return Err(AuthUnavailable::NeedsConsent);
        }
        match self.refresh_locked(&state, id, true).await {
            Ok(Some(entry)) => Ok(Some(state.authorization(&entry))),
            Ok(None) => Ok(None),
            Err(reason) => {
                state.refresh_failures.fetch_add(1, Ordering::Relaxed);
                Err(AuthUnavailable::RefreshFailed(reason))
            },
        }
    }

    /// Report an upstream credential rejection and, when the evidence matches
    /// OAuth token rejection semantics, run the same single-flight refresh path
    /// used by request-time and proactive refresh.
    pub async fn report_rejected(
        &self,
        id: &CredentialId,
        evidence: RejectionEvidence,
    ) -> RefreshOutcome {
        if !evidence.asks_for_refresh() {
            return RefreshOutcome::NotApplicable;
        }
        let Some(state) = self.state(id) else {
            return RefreshOutcome::NotApplicable;
        };
        if state.kind != AuthKind::OAuth {
            return RefreshOutcome::NotApplicable;
        }
        if state.needs_consent.load(Ordering::Relaxed) {
            return RefreshOutcome::RefreshFailed(AuthUnavailable::NeedsConsent.to_string());
        }

        match self.refresh_locked(&state, id, true).await {
            Ok(Some(_)) => RefreshOutcome::Refreshed,
            Ok(None) => RefreshOutcome::NoCredential,
            Err(reason) => {
                state.refresh_failures.fetch_add(1, Ordering::Relaxed);
                RefreshOutcome::RefreshFailed(reason)
            },
        }
    }

    /// Revoke an OAuth access token when this service knows a revocation
    /// endpoint for it, then delete the local credential entry.
    pub async fn revoke_and_delete(&self, id: &CredentialId) -> RevokeOutcome {
        let stored = match self.store.get(id) {
            Ok(stored) => stored,
            Err(error) => {
                return RevokeOutcome::DeleteFailed {
                    error: error.to_string(),
                };
            },
        };

        let upstream = match &stored {
            Some(entry) if entry.kind() == AuthKind::OAuth => {
                match self.state(id).and_then(|state| state.request.clone()) {
                    Some(request) => {
                        match self
                            .oauth
                            .revoke_access_token(request, entry.access_token().clone())
                            .await
                        {
                            Ok(OAuthRevokeOutcome::Revoked) => RevokeOutcome::Revoked,
                            Ok(OAuthRevokeOutcome::Unsupported) => RevokeOutcome::Unsupported,
                            Err(error) => RevokeOutcome::Failed {
                                error: error.to_string(),
                            },
                        }
                    },
                    None => RevokeOutcome::Unsupported,
                }
            },
            Some(_) | None => RevokeOutcome::LocalOnly,
        };

        if let Err(error) = self.store.delete(id) {
            return RevokeOutcome::DeleteFailed {
                error: error.to_string(),
            };
        }

        if let Some(state) = self.state(id) {
            state.current.store(None);
            state.needs_consent.store(false, Ordering::Relaxed);
            state.refresh_failures.store(0, Ordering::Relaxed);
        }

        upstream
    }

    /// Non-secret health for every bound credential.
    #[must_use]
    pub fn health(&self) -> Vec<CredentialStatus> {
        let now = OffsetDateTime::now_utc();
        self.states
            .iter()
            .map(|entry| {
                let id = entry.key().clone();
                self.status_from_state(id, entry.value(), now)
            })
            .collect()
    }

    /// Non-secret health for one bound credential.
    #[must_use]
    pub fn status(&self, id: &CredentialId) -> Option<CredentialStatus> {
        let state = self.state(id)?;
        Some(self.status_from_state(id.clone(), &state, OffsetDateTime::now_utc()))
    }

    /// Write a credential through the single store owner and refresh the cached
    /// entry so the next authorization sees it. Login flows use this instead of
    /// touching the store directly.
    pub fn store_entry(
        &self,
        id: &CredentialId,
        entry: CredentialEntry,
    ) -> Result<(), CredStoreError> {
        self.store.put(id, &entry)?;
        if let Some(state) = self.state(id)
            && entry.kind() == state.kind
        {
            state.current.store(Some(Arc::new(entry)));
            state.needs_consent.store(false, Ordering::Relaxed);
            state.refresh_failures.store(0, Ordering::Relaxed);
        }
        // The write may have moved this credential's expiry (or healed a
        // missing one); wake the refresh loop to recompute its deadline.
        self.refresh_notify.notify_one();
        Ok(())
    }

    /// Drop the cached entry for `id`, re-read/refresh it from the store, and
    /// return the refreshed non-secret status. `None` means no mounted provider
    /// has bound this credential with the service.
    pub async fn reload(&self, id: &CredentialId) -> Option<CredentialStatus> {
        let state = self.state(id)?;
        state.current.store(None);
        let _ = self.authorization(id).await;
        self.refresh_notify.notify_one();
        self.status(id)
    }

    fn state(&self, id: &CredentialId) -> Option<Arc<CredentialState>> {
        self.states.get(id).map(|slot| Arc::clone(slot.value()))
    }

    fn status_from_state(
        &self,
        id: CredentialId,
        state: &CredentialState,
        now: OffsetDateTime,
    ) -> CredentialStatus {
        let cached = self.current_entry(state, &id).ok().flatten();
        let (health, expires_at, scopes) = match cached {
            None => (CredentialHealth::Missing, None, Vec::new()),
            Some(credential) => {
                let failures = state.refresh_failures.load(Ordering::Relaxed);
                (
                    state.health(&credential, now, failures),
                    credential.expires_at(),
                    credential.scopes().to_vec(),
                )
            },
        };
        CredentialStatus {
            id,
            health,
            expires_at,
            scopes,
        }
    }

    /// The current entry for `id`: the cached value, else a store read that
    /// warms the cache. Never refreshes.
    fn current_entry(
        &self,
        state: &CredentialState,
        id: &CredentialId,
    ) -> Result<Option<CredentialEntry>, CredStoreError> {
        if let Some(cached) = state.current.load_full() {
            return Ok(Some((*cached).clone()));
        }
        let stored = self
            .store
            .get(id)?
            .filter(|entry| entry.kind() == state.kind);
        if let Some(entry) = &stored {
            state.current.store(Some(Arc::new(entry.clone())));
        }
        Ok(stored)
    }

    /// Single-flight refresh: concurrent callers for one id coalesce onto one
    /// [`do_refresh`], keyed by the credential storage key.
    ///
    /// The caller's view of the credential is snapshotted BEFORE joining the
    /// flight: a forced refresh means "the token I saw was rejected or is
    /// due", and a flight that starts after a concurrent rotation must
    /// compare the store against that pre-flight snapshot, not against
    /// `state.current` (which the previous leader already rotated), or it
    /// spends the fresh rotated refresh token on a needless endpoint call.
    ///
    /// [`do_refresh`]: CredentialService::do_refresh
    async fn refresh_locked(
        &self,
        state: &CredentialState,
        id: &CredentialId,
        force: bool,
    ) -> Result<Option<CredentialEntry>, String> {
        let observed = state.current.load_full();
        let key = id.storage_key();
        let result = self
            .refreshes
            .work(&key, async move {
                self.do_refresh(state, id, force, observed).await
            })
            .await;
        match result {
            Ok(entry) => Ok(entry),
            Err(Some(reason)) => Err(reason),
            Err(None) => Err("refresh leader failed".to_string()),
        }
    }

    async fn do_refresh(
        &self,
        state: &CredentialState,
        id: &CredentialId,
        force: bool,
        observed: Option<Arc<CredentialEntry>>,
    ) -> Result<Option<CredentialEntry>, String> {
        let now = OffsetDateTime::now_utc();
        let stored = self
            .store
            .get(id)
            .map_err(|error| error.to_string())?
            .filter(|entry| entry.kind() == AuthKind::OAuth);
        let Some(stored) = stored else {
            state.current.store(None);
            return Ok(None);
        };

        // A concurrently-written stored entry that is already fresh (and, when
        // forcing, differs from the token the caller observed before joining
        // the flight) supersedes an endpoint call.
        if Self::stored_satisfies(&stored, observed.as_deref(), now, force) {
            state.current.store(Some(Arc::new(stored.clone())));
            return Ok(Some(stored));
        }

        let Some(refresh_token) = stored.refresh_token() else {
            state.current.store(Some(Arc::new(stored.clone())));
            return Err(format!(
                "OAuth credential {id} cannot be refreshed and requires re-authentication"
            ));
        };
        let Some(request) = state.request.clone() else {
            return Err(format!("OAuth credential {id} is not bound for refresh"));
        };

        match self.oauth.refresh(request, refresh_token).await {
            Ok(refreshed) => {
                self.store
                    .put(id, &refreshed)
                    .map_err(|error| error.to_string())?;
                state.current.store(Some(Arc::new(refreshed.clone())));
                state.needs_consent.store(false, Ordering::Relaxed);
                state.refresh_failures.store(0, Ordering::Relaxed);
                Ok(Some(refreshed))
            },
            Err(AuthError::TokenEndpoint { error, .. }) if error == "invalid_grant" => {
                state.current.store(Some(Arc::new(stored)));
                state.needs_consent.store(true, Ordering::Relaxed);
                Err("OAuth refresh token was rejected".to_string())
            },
            Err(error) => Err(error.to_string()),
        }
    }

    /// Spawn the proactive OAuth refresh loop: it refreshes every bound
    /// OAuth credential before it enters [`REFRESH_WINDOW`], so a request-path
    /// [`authorization`](Self::authorization) call almost never has to await a
    /// live refresh. Never returns on its own; abort the returned handle to
    /// stop it (the daemon does this on shutdown).
    #[must_use]
    pub fn spawn_refresh_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let service = Arc::clone(self);
        tokio::spawn(async move { service.run_refresh_loop().await })
    }

    async fn run_refresh_loop(self: Arc<Self>) {
        let mut rng = SmallRng::seed_from_u64(REFRESH_LOOP_SEED);
        loop {
            let Some((id, deadline)) = self.earliest_oauth_deadline() else {
                // Nothing to schedule around yet (no OAuth credential is
                // bound, or none carries an expiry). `bind_oauth`
                // wakes this the moment that changes.
                self.refresh_notify.notified().await;
                continue;
            };

            let now = OffsetDateTime::now_utc();
            let remaining = deadline - now;
            let remaining = Duration::try_from(remaining).unwrap_or(Duration::ZERO);
            let sleep_for = (remaining + Self::refresh_jitter(&mut rng, remaining))
                .max(REFRESH_LOOP_MIN_INTERVAL);

            tokio::select! {
                () = tokio::time::sleep(sleep_for) => {},
                () = self.refresh_notify.notified() => continue,
            }

            // The deadline is `expires_at - REFRESH_WINDOW`: by construction,
            // reaching it means this credential is due now, so force the
            // rotation through the same single-flight `refresh` the rejection
            // path uses, rather than re-deriving "is it still fresh" from
            // `authorization` (which would just repeat the check that placed
            // this deadline here). `refresh`'s same-token comparison still
            // no-ops if another caller already rotated it concurrently.
            if let Err(reason) = self.refresh(&id).await {
                warn!(credential = %id, error = %reason, "proactive credential refresh failed");
            }
        }
    }

    /// The nearest `expires_at - REFRESH_WINDOW` across every bound
    /// OAuth credential the loop can still rotate, and which credential it
    /// belongs to. Credentials in `NeedsConsent` or without a refresh token
    /// are excluded: their deadline is already past and can never advance, so
    /// scheduling around them would pin the minimum forever and starve every
    /// other credential of proactive refresh. Reads only the in-memory cache
    /// (never the store): any external write reaches this cache through
    /// `store_entry` or `reload`, both of which wake the loop, so a stale
    /// read here is corrected on the next wake rather than by polling the
    /// store on a timer.
    pub(crate) fn earliest_oauth_deadline(&self) -> Option<(CredentialId, OffsetDateTime)> {
        let window = time::Duration::try_from(REFRESH_WINDOW)
            .expect("REFRESH_WINDOW fits in time::Duration");
        self.states
            .iter()
            .filter(|entry| {
                entry.value().kind == AuthKind::OAuth
                    && !entry.value().needs_consent.load(Ordering::Relaxed)
            })
            .filter_map(|entry| {
                let current = entry.value().current.load_full()?;
                current.refresh_token()?;
                let expires_at = current.expires_at()?;
                Some((entry.key().clone(), expires_at - window))
            })
            .min_by_key(|(_, deadline)| *deadline)
    }

    /// Additive jitter up to [`REFRESH_JITTER_FRACTION`] of `base`, from a
    /// deterministic PRNG so a test driving the loop under paused time sees a
    /// reproducible sequence of sleeps.
    fn refresh_jitter(rng: &mut SmallRng, base: Duration) -> Duration {
        base.mul_f64(rng.random::<f64>() * REFRESH_JITTER_FRACTION)
    }

    fn stored_satisfies(
        stored: &CredentialEntry,
        current: Option<&CredentialEntry>,
        now: OffsetDateTime,
        force: bool,
    ) -> bool {
        if !Self::is_fresh(stored, now) {
            return false;
        }
        if !force {
            return true;
        }
        current.is_none_or(|current| !same_oauth_token(stored, current))
    }
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
    name.trim().eq_ignore_ascii_case("error") && unquote(value.trim()) == "invalid_token"
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}
