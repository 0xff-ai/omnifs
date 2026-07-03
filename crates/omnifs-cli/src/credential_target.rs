use anyhow::{Context, anyhow};
use omnifs_workspace::authn::{AccountId, CredentialId, SchemeId as AuthSchemeId};
use omnifs_workspace::creds::{CredentialEntry, CredentialStore};
use omnifs_workspace::ids::ProviderName;
use omnifs_workspace::mounts::Auth;
use omnifs_workspace::mounts::Spec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialTarget {
    Internal(CredentialId),
    None,
}

impl CredentialTarget {
    pub(crate) fn for_scheme(
        config: &Spec,
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

    pub(crate) fn for_mount(config: &Spec) -> Self {
        config.auth.as_ref().map_or(Self::None, |auth| {
            Self::for_configured_auth(config, auth, auth.scheme()).unwrap_or(Self::None)
        })
    }

    pub(crate) fn keys(&self) -> Vec<&CredentialId> {
        match self {
            Self::Internal(key) => vec![key],
            Self::None => Vec::new(),
        }
    }

    pub(crate) fn primary_key(&self) -> Option<&CredentialId> {
        match self {
            Self::Internal(key) => Some(key),
            Self::None => None,
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
        config: &Spec,
        auth: &Auth,
        scheme: Option<&str>,
    ) -> anyhow::Result<Self> {
        let scheme = scheme.ok_or_else(|| anyhow!("missing auth.scheme"))?;
        // `account` is derived from `auth` inside `internal_from_parts`.
        Self::internal_from_parts(config, Some(auth), scheme, None)
    }

    fn internal_from_parts(
        config: &Spec,
        auth: Option<&Auth>,
        scheme: &str,
        account: Option<&str>,
    ) -> anyhow::Result<Self> {
        let provider_id = config.provider_name().clone();
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
