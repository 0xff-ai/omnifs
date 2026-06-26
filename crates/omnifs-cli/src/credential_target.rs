use std::fmt;

use anyhow::{Context, anyhow};
use omnifs_core::{AccountId, AuthSchemeId, CredentialId, ProviderName};
use omnifs_creds::{CredentialEntry, CredentialStore};
use omnifs_mount::Auth;
use omnifs_mount::mounts::Resolved;

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

impl CredentialTarget {
    pub(crate) fn for_scheme(
        config: &Resolved,
        auth: Option<&Auth>,
        scheme: &str,
        account: Option<&str>,
    ) -> anyhow::Result<Self> {
        Self::internal_from_parts(config, auth, scheme, account)
    }

    pub(crate) fn for_static_import(
        provider_id: &str,
        scheme: &str,
        account: Option<&str>,
    ) -> anyhow::Result<Self> {
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

    pub(crate) fn for_mount(config: &Resolved) -> Self {
        config.spec.auth.first().map_or(Self::None, |auth| {
            Self::for_configured_auth(config, auth, auth.scheme(), auth.account())
                .unwrap_or(Self::None)
        })
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

    pub(crate) fn for_configured_auth(
        config: &Resolved,
        auth: &Auth,
        scheme: Option<&str>,
        account: Option<&str>,
    ) -> anyhow::Result<Self> {
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
        let scheme = scheme.ok_or_else(|| anyhow!("missing auth.scheme"))?;
        Self::internal_from_parts(config, Some(auth), scheme, account)
    }

    fn internal_from_parts(
        config: &Resolved,
        auth: Option<&Auth>,
        scheme: &str,
        account: Option<&str>,
    ) -> anyhow::Result<Self> {
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
