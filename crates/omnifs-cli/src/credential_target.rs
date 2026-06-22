use std::fmt;

use anyhow::Context;
use omnifs_core::{AccountId, AuthSchemeId, CredentialId, IdError, ProviderName};
use omnifs_creds::{CredentialEntry, CredentialStore};
use omnifs_mount::Auth;
use omnifs_mount::mounts::Resolved;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialTarget {
    Internal(CredentialId),
    External(ExternalCredentialSource),
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExternalCredentialSource {
    TokenFile(String),
    TokenEnv(String),
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub(crate) enum CredentialTargetError {
    #[error("missing auth.scheme")]
    MissingScheme,
    #[error(transparent)]
    InvalidKey(#[from] IdError),
}

impl CredentialTarget {
    pub(crate) fn for_scheme(
        config: &Resolved,
        auth: Option<&Auth>,
        scheme: &str,
        account: Option<&str>,
    ) -> Result<Self, CredentialTargetError> {
        Self::internal_from_parts(config, auth, scheme, account)
    }

    pub(crate) fn for_static_import(
        provider_id: &str,
        scheme: &str,
        account: Option<&str>,
    ) -> Result<Self, CredentialTargetError> {
        let provider_id = ProviderName::new(provider_id)?;
        let scheme = AuthSchemeId::new(scheme)?;
        let account = account
            .map(AccountId::new)
            .transpose()?
            .unwrap_or_else(AccountId::default_account);
        Ok(CredentialTarget::Internal(CredentialId::from_parts(
            provider_id,
            scheme,
            account,
        )))
    }

    pub(crate) fn from_resolved_mount(config: &Resolved) -> Result<Self, CredentialTargetError> {
        let Some(auth) = config.spec.auth.first() else {
            return Ok(Self::None);
        };

        if let Some(token_file) = auth.token_file() {
            return Ok(Self::External(ExternalCredentialSource::TokenFile(
                token_file.to_owned(),
            )));
        }
        if let Some(token_env) = auth.token_env() {
            return Ok(Self::External(ExternalCredentialSource::TokenEnv(
                token_env.to_owned(),
            )));
        }

        let Some(scheme) = auth.scheme() else {
            return Err(CredentialTargetError::MissingScheme);
        };
        Self::internal_from_parts(config, Some(auth), scheme, auth.account())
    }

    pub(crate) fn for_mount(config: &Resolved) -> Self {
        if config.spec.auth.is_empty() {
            return Self::None;
        }

        let auth = &config.spec.auth[0];
        if let Some(token_file) = auth.token_file() {
            return Self::External(ExternalCredentialSource::TokenFile(token_file.to_owned()));
        }
        if let Some(token_env) = auth.token_env() {
            return Self::External(ExternalCredentialSource::TokenEnv(token_env.to_owned()));
        }

        let Some(scheme) = auth.scheme() else {
            return Self::None;
        };
        let Ok(provider_id) = ProviderName::new(&config.provider_name) else {
            return Self::None;
        };
        let account = auth
            .account()
            .map(AccountId::new)
            .transpose()
            .ok()
            .flatten()
            .unwrap_or_else(AccountId::default_account);
        let Ok(scheme_key) = AuthSchemeId::new(scheme) else {
            return Self::None;
        };
        Self::Internal(CredentialId::from_parts(provider_id, scheme_key, account))
    }

    pub(crate) fn keys(&self) -> Vec<&CredentialId> {
        match self {
            Self::Internal(key) => vec![key],
            Self::External(_) | Self::None => Vec::new(),
        }
    }

    pub(crate) fn primary_key(&self) -> Option<&CredentialId> {
        match self {
            Self::Internal(key) => Some(key),
            Self::External(_) | Self::None => None,
        }
    }

    pub(crate) fn delete_from(
        &self,
        store: &dyn CredentialStore,
        mount_name: &str,
    ) -> anyhow::Result<()> {
        for key in self.keys() {
            store
                .delete(key)
                .with_context(|| format!("delete credential for mount `{mount_name}`"))?;
            anstream::println!("Deleted credential `{}`", key.storage_key());
        }
        Ok(())
    }

    pub(crate) fn lookup(
        &self,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<Option<CredentialEntry>> {
        let Self::Internal(key) = self else {
            return Ok(None);
        };
        store.get(key).map_err(Into::into)
    }

    fn internal_from_parts(
        config: &Resolved,
        auth: Option<&Auth>,
        scheme: &str,
        account: Option<&str>,
    ) -> Result<Self, CredentialTargetError> {
        let provider_id = ProviderName::new(&config.provider_name)?;
        let account = account
            .or_else(|| auth.and_then(Auth::account))
            .map(AccountId::new)
            .transpose()?
            .unwrap_or_else(AccountId::default_account);
        let scheme = AuthSchemeId::new(scheme)?;
        Ok(CredentialTarget::Internal(CredentialId::from_parts(
            provider_id,
            scheme,
            account,
        )))
    }
}

impl fmt::Display for ExternalCredentialSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TokenFile(path) => write!(f, "token_file={path}"),
            Self::TokenEnv(name) => write!(f, "token_env={name}"),
        }
    }
}
