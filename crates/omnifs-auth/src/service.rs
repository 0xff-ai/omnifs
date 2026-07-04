//! Proactive, fail-closed credential ownership.
//!
//! [`CredentialService`] is the single host-side owner of credential store
//! access, the one expiry predicate ([`is_fresh`]), and OAuth refresh. A mount's
//! injector ([`crate`] consumers in `omnifs-engine`) registers each credential
//! it needs, then asks for [`CredentialService::authorization`] per request. The
//! service reads the store, refreshes synchronously inside the refresh window,
//! coalesces concurrent refreshes per credential, and NEVER returns a stale or
//! absent credential as success. Token material never leaves the service except
//! as the composed [`HeaderMaterial`] the injector places on the wire; it is
//! never logged and never placed on a status or wire type.

use crate::client::OAuthClient;
use crate::error::AuthError;
use crate::request::OAuthRequest;
use arc_swap::ArcSwapOption;
use async_singleflight::Group;
use dashmap::DashMap;
use omnifs_workspace::authn::{AuthKind, CredentialId};
use omnifs_workspace::creds::{CredStoreError, CredentialEntry, CredentialStore};
use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use time::OffsetDateTime;

/// The single freshness margin. A credential is usable only when at least this
/// long remains before expiry; inside the window it is refreshed (OAuth) or
/// treated as unavailable. This is the one place the margin lives: the OAuth
/// mint path stamps the real `expires_at` with no baked-in skew, and the margin
/// is applied here, at check time.
// Expressed in seconds deliberately: this is the "60-second refresh window",
// not "1 minute" of wall-clock policy.
#[allow(clippy::duration_suboptimal_units)]
pub const REFRESH_WINDOW: Duration = Duration::from_secs(60);

/// The one expiry predicate. Every credential expiry decision (emit, refresh,
/// health) routes through here. A credential with no expiry is always fresh.
#[must_use]
pub fn is_fresh(entry: &CredentialEntry, now: OffsetDateTime) -> bool {
    let window =
        time::Duration::try_from(REFRESH_WINDOW).expect("REFRESH_WINDOW fits in time::Duration");
    entry
        .expires_at()
        .is_none_or(|expires_at| expires_at - now > window)
}

/// The resolved auth header the injector places on a request. Holds the secret
/// value (already prefixed, e.g. `Bearer <token>`) as a [`SecretString`] so it
/// is redacted in logs; the injector exposes it only at the wire boundary.
#[derive(Debug, Clone)]
pub struct HeaderMaterial {
    name: String,
    value: SecretString,
}

impl HeaderMaterial {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Expose the composed header value for placement on the wire. This is the
    /// single controlled disclosure of token material.
    #[must_use]
    pub fn expose_value(&self) -> &str {
        self.value.expose_secret()
    }
}

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

/// Non-secret health of one registered credential. NEVER holds token material.
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

/// Per-credential state: how to compose its header, how to refresh it (OAuth),
/// and the last-known entry cached in memory. `current` is the token the
/// injector last resolved; `do_refresh` compares against it to decide whether a
/// forced refresh must hit the endpoint or can adopt a newer stored entry.
struct CredentialState {
    kind: AuthKind,
    header_name: String,
    value_prefix: String,
    /// `Some` for OAuth (carries endpoints, client credentials, refresh
    /// params); `None` for static tokens, which never refresh.
    request: Option<OAuthRequest>,
    current: ArcSwapOption<CredentialEntry>,
    refresh_failures: AtomicU32,
}

/// Proactive owner of credential store access, expiry, and OAuth refresh.
///
/// One instance is shared across every mount in a host: mounts that resolve to
/// the same [`CredentialId`] share one refresh state, so a single refresh serves
/// them all. Registration is idempotent (first registration wins).
pub struct CredentialService {
    store: Arc<dyn CredentialStore>,
    oauth: OAuthClient,
    states: DashMap<CredentialId, Arc<CredentialState>>,
    /// Per-credential single-flight: concurrent refreshes for one id coalesce
    /// onto one endpoint call.
    refreshes: Group<String, Option<CredentialEntry>, String>,
}

impl CredentialService {
    #[must_use]
    pub fn new(store: Arc<dyn CredentialStore>, oauth: OAuthClient) -> Self {
        Self {
            store,
            oauth,
            states: DashMap::new(),
            refreshes: Group::new(),
        }
    }

    /// Register a static-token credential and its header shape. Reads the store
    /// once to warm the in-memory entry.
    pub fn register_static(&self, id: CredentialId, header_name: String, value_prefix: String) {
        self.register(id, AuthKind::StaticToken, header_name, value_prefix, None);
    }

    /// Register an OAuth credential. The header shape and refresh parameters both
    /// come from the request's scheme.
    pub fn register_oauth(&self, id: CredentialId, request: OAuthRequest) {
        let scheme = request.scheme();
        let header_name = scheme
            .inject_header_name
            .clone()
            .unwrap_or_else(|| "Authorization".to_string());
        let value_prefix = scheme.inject_value_prefix.clone();
        self.register(
            id,
            AuthKind::OAuth,
            header_name,
            value_prefix,
            Some(request),
        );
    }

    fn register(
        &self,
        id: CredentialId,
        kind: AuthKind,
        header_name: String,
        value_prefix: String,
        request: Option<OAuthRequest>,
    ) {
        use dashmap::mapref::entry::Entry;
        let Entry::Vacant(slot) = self.states.entry(id) else {
            // First registration wins: a live refresh state is never clobbered.
            return;
        };
        let current = self
            .store
            .get(slot.key())
            .ok()
            .flatten()
            .filter(|entry| entry.kind() == kind)
            .map(Arc::new);
        slot.insert(Arc::new(CredentialState {
            kind,
            header_name,
            value_prefix,
            request,
            current: ArcSwapOption::new(current),
            refresh_failures: AtomicU32::new(0),
        }));
    }

    /// Resolve a usable auth header for `id`, refreshing synchronously when the
    /// credential is inside the refresh window. Fails closed: a missing, expired,
    /// or unrefreshable credential returns an error, never a stale header.
    pub async fn authorization(
        &self,
        id: &CredentialId,
    ) -> Result<HeaderMaterial, AuthUnavailable> {
        let Some(state) = self.state(id) else {
            return Err(AuthUnavailable::Missing);
        };
        let Some(entry) = self
            .current_entry(&state, id)
            .map_err(|error| AuthUnavailable::RefreshFailed(error.to_string()))?
        else {
            return Err(AuthUnavailable::Missing);
        };

        if state.kind == AuthKind::StaticToken {
            return Ok(compose(&state, &entry));
        }

        if is_fresh(&entry, OffsetDateTime::now_utc()) {
            return Ok(compose(&state, &entry));
        }

        match self.refresh_locked(&state, id, false).await {
            Ok(Some(fresh)) => Ok(compose(&state, &fresh)),
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
    pub fn cached_authorization(&self, id: &CredentialId) -> Option<HeaderMaterial> {
        let state = self.state(id)?;
        let entry = self.current_entry(&state, id).ok().flatten()?;
        is_fresh(&entry, OffsetDateTime::now_utc()).then(|| compose(&state, &entry))
    }

    /// Force a refresh of `id` (an upstream rejected the current token).
    /// `Ok(Some)` rotated the token, `Ok(None)` means no credential is stored,
    /// `Err` means the refresh failed (fail closed).
    pub async fn refresh(
        &self,
        id: &CredentialId,
    ) -> Result<Option<HeaderMaterial>, AuthUnavailable> {
        let Some(state) = self.state(id) else {
            return Ok(None);
        };
        if state.kind != AuthKind::OAuth {
            return Ok(None);
        }
        match self.refresh_locked(&state, id, true).await {
            Ok(Some(entry)) => Ok(Some(compose(&state, &entry))),
            Ok(None) => Ok(None),
            Err(reason) => {
                state.refresh_failures.fetch_add(1, Ordering::Relaxed);
                Err(AuthUnavailable::RefreshFailed(reason))
            },
        }
    }

    /// Non-secret health for every registered credential.
    #[must_use]
    pub fn health(&self) -> Vec<CredentialStatus> {
        let now = OffsetDateTime::now_utc();
        self.states
            .iter()
            .map(|entry| {
                let id = entry.key().clone();
                let state = entry.value();
                let cached = self.current_entry(state, &id).ok().flatten();
                let (health, expires_at, scopes) = match cached {
                    None => (CredentialHealth::Missing, None, Vec::new()),
                    Some(credential) => {
                        let failures = state.refresh_failures.load(Ordering::Relaxed);
                        (
                            classify(state, &credential, now, failures),
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
            })
            .collect()
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
        }
        Ok(())
    }

    /// Drop the cached entry for `id` and re-read/refresh it from the store.
    pub async fn reload(&self, id: &CredentialId) {
        if let Some(state) = self.state(id) {
            state.current.store(None);
        }
        let _ = self.authorization(id).await;
    }

    fn state(&self, id: &CredentialId) -> Option<Arc<CredentialState>> {
        self.states.get(id).map(|slot| Arc::clone(slot.value()))
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
    /// [`do_refresh`]: CredentialService::do_refresh
    async fn refresh_locked(
        &self,
        state: &CredentialState,
        id: &CredentialId,
        force: bool,
    ) -> Result<Option<CredentialEntry>, String> {
        let key = id.storage_key();
        let result = self
            .refreshes
            .work(&key, async move { self.do_refresh(state, id, force).await })
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
    ) -> Result<Option<CredentialEntry>, String> {
        let now = OffsetDateTime::now_utc();
        let current = state.current.load_full();
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
        // forcing, differs from the token we last used) supersedes an endpoint
        // call.
        if stored_satisfies(&stored, current.as_deref(), now, force) {
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
            return Err(format!(
                "OAuth credential {id} is not registered for refresh"
            ));
        };

        match self.oauth.refresh(request, refresh_token).await {
            Ok(refreshed) => {
                self.store
                    .put(id, &refreshed)
                    .map_err(|error| error.to_string())?;
                state.current.store(Some(Arc::new(refreshed.clone())));
                Ok(Some(refreshed))
            },
            Err(AuthError::TokenEndpoint { error, .. }) if error == "invalid_grant" => {
                self.store.delete(id).map_err(|error| error.to_string())?;
                state.current.store(None);
                Err("OAuth refresh token was rejected".to_string())
            },
            Err(error) => Err(error.to_string()),
        }
    }
}

fn compose(state: &CredentialState, entry: &CredentialEntry) -> HeaderMaterial {
    let value = format!(
        "{}{}",
        state.value_prefix,
        entry.access_token().expose_secret()
    );
    HeaderMaterial {
        name: state.header_name.clone(),
        value: SecretString::from(value),
    }
}

fn classify(
    state: &CredentialState,
    entry: &CredentialEntry,
    now: OffsetDateTime,
    failures: u32,
) -> CredentialHealth {
    if state.kind == AuthKind::StaticToken {
        return CredentialHealth::StaticUnvalidated;
    }
    if failures > 0 && !is_fresh(entry, now) {
        return CredentialHealth::RefreshFailed { attempts: failures };
    }
    if entry.is_expired_at(now) {
        return if entry.refresh_token().is_some() {
            CredentialHealth::Expired
        } else {
            CredentialHealth::NeedsConsent
        };
    }
    if is_fresh(entry, now) {
        CredentialHealth::Ready
    } else {
        CredentialHealth::ExpiringSoon
    }
}

fn stored_satisfies(
    stored: &CredentialEntry,
    current: Option<&CredentialEntry>,
    now: OffsetDateTime,
    force: bool,
) -> bool {
    if !is_fresh(stored, now) {
        return false;
    }
    if !force {
        return true;
    }
    current.is_none_or(|current| !same_oauth_token(stored, current))
}

fn same_oauth_token(left: &CredentialEntry, right: &CredentialEntry) -> bool {
    left.access_token().expose_secret() == right.access_token().expose_secret()
        && left
            .refresh_token()
            .map(|token| token.expose_secret().to_owned())
            == right
                .refresh_token()
                .map(|token| token.expose_secret().to_owned())
}
