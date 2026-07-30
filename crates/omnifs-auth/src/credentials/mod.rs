use secrecy::SecretString;
use time::OffsetDateTime;

use crate::AuthKind;

/// One durable host-managed HTTP credential entry.
#[derive(Debug, Clone)]
pub struct CredentialEntry {
    kind: AuthKind,
    value: SecretString,
    scopes: Vec<String>,
    /// Human-readable identity reported by the upstream API at validation time.
    upstream_identity: Option<String>,
    refresh_token: Option<SecretString>,
    expires_at: Option<OffsetDateTime>,
    token_type: String,
}

impl CredentialEntry {
    pub fn static_token(access_token: SecretString) -> Self {
        Self {
            kind: AuthKind::StaticToken,
            value: access_token,
            scopes: vec![],
            upstream_identity: None,
            refresh_token: None,
            expires_at: None,
            token_type: "Bearer".to_owned(),
        }
    }

    pub fn oauth(
        access_token: SecretString,
        refresh_token: Option<SecretString>,
        expires_at: Option<OffsetDateTime>,
        token_type: impl Into<String>,
        scopes: Vec<String>,
    ) -> Self {
        Self {
            kind: AuthKind::OAuth,
            value: access_token,
            scopes,
            upstream_identity: None,
            refresh_token,
            expires_at,
            token_type: Self::normalize_token_type(token_type.into()),
        }
    }

    pub(crate) fn merge_oauth_refresh(
        &self,
        access_token: SecretString,
        refresh_token: Option<SecretString>,
        expires_at: Option<OffsetDateTime>,
        token_type: impl Into<String>,
        scopes: Option<Vec<String>>,
    ) -> Self {
        debug_assert_eq!(self.kind, AuthKind::OAuth);
        Self {
            kind: AuthKind::OAuth,
            value: access_token,
            scopes: scopes.unwrap_or_else(|| self.scopes.clone()),
            upstream_identity: self.upstream_identity.clone(),
            refresh_token: refresh_token.or_else(|| self.refresh_token.clone()),
            expires_at,
            token_type: Self::normalize_token_type(token_type.into()),
        }
    }

    pub fn kind(&self) -> AuthKind {
        self.kind
    }

    pub fn access_token(&self) -> &SecretString {
        &self.value
    }

    pub fn refresh_token(&self) -> Option<SecretString> {
        self.refresh_token.clone()
    }

    pub fn expires_at(&self) -> Option<OffsetDateTime> {
        self.expires_at
    }

    pub fn is_expired_at(&self, now: OffsetDateTime) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }

    pub fn token_type(&self) -> &str {
        &self.token_type
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    pub fn upstream_identity(&self) -> Option<&str> {
        self.upstream_identity.as_deref()
    }

    pub fn set_upstream_identity(&mut self, upstream_identity: Option<String>) {
        self.upstream_identity = upstream_identity;
    }

    fn normalize_token_type(token_type: String) -> String {
        if token_type.is_empty() {
            "Bearer".to_owned()
        } else {
            token_type
        }
    }
}
