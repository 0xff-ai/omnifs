//! Startup-owned credential bindings and live OAuth refresh.

use crate::client::OAuthClient;
use crate::error::AuthError;
use crate::request::OAuthRequest;
use arc_swap::ArcSwapOption;
use async_singleflight::Group;
use omnifs_workspace::authn::{AuthKind, CredentialId};
use omnifs_workspace::creds::{CredStoreError, CredentialEntry, CredentialStore};
use secrecy::ExposeSecret;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

    #[must_use]
    pub fn needs_attention(&self) -> bool {
        !matches!(self, Self::Ready | Self::StaticUnvalidated)
    }
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
    message: String,
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
        kind: AuthKind,
        domains: Vec<String>,
        header_name: String,
        value_prefix: String,
        request: Option<OAuthRequest>,
        current: Option<CredentialEntry>,
    ) -> Self {
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

    #[must_use]
    pub fn warning(&self) -> Option<String> {
        let health = self.health();
        health
            .needs_attention()
            .then(|| format!("credential {} is {health:?}", self.id))
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
        match self.refresh(true).await {
            Ok(Some(_)) => RefreshOutcome::Refreshed,
            Ok(None) => RefreshOutcome::NoCredential,
            Err(AuthUnavailable::Missing) => RefreshOutcome::NoCredential,
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
}

/// Shared durable store and OAuth transport. Mount bindings retain the loaded
/// entry and runtime metadata; this service has no mount registry.
pub struct CredentialService {
    store: Arc<dyn CredentialStore>,
    oauth: OAuthClient,
    refreshes: Group<String, Option<CredentialEntry>, RefreshFailure>,
}

impl CredentialService {
    #[must_use]
    pub fn new(store: Arc<dyn CredentialStore>, oauth: OAuthClient) -> Self {
        Self {
            store,
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
        let current = self.store.get(&id)?;
        validate_kind(&id, AuthKind::StaticToken, current.as_ref())?;
        Ok(AuthBinding::new(
            Arc::clone(self),
            id,
            AuthKind::StaticToken,
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
        let current = self.store.get(&id)?;
        validate_kind(&id, AuthKind::OAuth, current.as_ref())?;
        Ok(AuthBinding::new(
            Arc::clone(self),
            id,
            AuthKind::OAuth,
            domains,
            header_name,
            value_prefix,
            Some(request),
            current,
        ))
    }

    /// Store an imported credential. Existing bindings intentionally do not
    /// observe this write because auth changes take effect at next startup.
    pub fn store_entry(
        &self,
        id: &CredentialId,
        entry: CredentialEntry,
    ) -> Result<(), CredStoreError> {
        self.store.put(id, &entry)
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
            Ok(result) => result,
            Err(Some(error)) => Err(error),
            Err(None) => Err(RefreshFailure {
                needs_consent: false,
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
        let stored = self
            .store
            .get(id)
            .map_err(|error| RefreshFailure {
                needs_consent: false,
                message: error.to_string(),
            })?
            .filter(|entry| entry.kind() == AuthKind::OAuth);
        let Some(stored) = stored else {
            return Ok(None);
        };
        if Self::stored_satisfies(&stored, observed, OffsetDateTime::now_utc(), force) {
            return Ok(Some(stored));
        }
        let Some(refresh_token) = stored.refresh_token() else {
            return Err(RefreshFailure {
                needs_consent: true,
                message: format!("OAuth credential {id} requires re-authentication"),
            });
        };
        match self.oauth.refresh(request.clone(), refresh_token).await {
            Ok(refreshed) => {
                self.store
                    .put(id, &refreshed)
                    .map_err(|error| RefreshFailure {
                        needs_consent: false,
                        message: error.to_string(),
                    })?;
                Ok(Some(refreshed))
            },
            Err(AuthError::TokenEndpoint { error, .. }) if error == "invalid_grant" => {
                Err(RefreshFailure {
                    needs_consent: true,
                    message: "OAuth refresh token was rejected".to_string(),
                })
            },
            Err(error) => Err(RefreshFailure {
                needs_consent: false,
                message: error.to_string(),
            }),
        }
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
