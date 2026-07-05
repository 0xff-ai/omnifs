use anyhow::anyhow;
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
        // The account-defaulting derivation lives solely in `CredentialId::for_mount`.
        // An explicit account (e.g. `--account`) or an authless scheme uses the
        // shared explicit-parts constructor instead.
        match (account, auth) {
            (Some(account), _) => {
                let id = CredentialId::new(config.provider_name().as_str(), scheme, account)?;
                Ok(Self::Internal(id))
            },
            (None, Some(auth)) => Self::for_configured_auth(config, auth, Some(scheme)),
            (None, None) => {
                let id = CredentialId::new(
                    config.provider_name().as_str(),
                    scheme,
                    AccountId::default_account().as_str(),
                )?;
                Ok(Self::Internal(id))
            },
        }
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
        let id = CredentialId::for_mount(config.provider_name(), auth, scheme)?;
        Ok(Self::Internal(id))
    }
}
