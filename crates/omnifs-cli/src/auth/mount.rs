//! Mount-scoped auth loading, init auth selection, and OAuth request construction.

use anyhow::{Context, anyhow};
use omnifs_auth::OAuthRequest;
use omnifs_creds::{CredentialEntry, CredentialStore};
use omnifs_host::auth::oauth_request_from_config;
use omnifs_host::mounts::Resolved;
use omnifs_mount_schema::Auth;
use omnifs_mount_schema::{
    AuthInject, AuthManifest, ManifestAuthScheme, ManifestStaticTokenScheme, ProviderManifest,
};

use super::manifest_view::AuthManifestView;
use crate::catalog::ProviderCatalog;
use crate::credential_target::CredentialTarget;
use omnifs_core::MountName;

/// Auth mode chosen during `omnifs init` before a mount config exists on disk.
#[derive(Clone, Debug)]
pub(crate) struct AuthSelection {
    pub(crate) auth_type: String,
    pub(crate) scheme: Option<String>,
    pub(crate) account: Option<String>,
}

impl AuthSelection {
    pub(crate) fn from_provider_default(manifest: &ProviderManifest) -> Option<Self> {
        let auth = manifest.auth.as_ref()?;
        let (key, scheme) = auth.default_scheme()?;
        let auth_type = match scheme {
            ManifestAuthScheme::StaticToken(_) => "static-token".to_string(),
            ManifestAuthScheme::Oauth(_) => "oauth".to_string(),
        };
        Some(Self {
            auth_type,
            scheme: Some(key.to_owned()),
            account: None,
        })
    }

    pub(crate) fn is_oauth(&self) -> bool {
        self.auth_type == "oauth"
    }

    pub(crate) fn promote_imported_static(
        mut self,
        auth_manifest: Option<&AuthManifest>,
        provider_id: &str,
    ) -> anyhow::Result<Self> {
        if !self.is_oauth() {
            return Ok(self);
        }
        let account = self.account.take();
        match AuthManifestView::new(auth_manifest).first_static_token_scheme_key() {
            Some(scheme) => Ok(Self {
                auth_type: "static-token".to_string(),
                scheme: Some(scheme),
                account,
            }),
            None => anyhow::bail!(
                "imported a static token for `{provider_id}`, but the provider declares no static-token scheme; remove the ambient credential or run OAuth"
            ),
        }
    }

    pub(crate) fn static_token_scheme<'a>(
        &self,
        manifest: &'a ProviderManifest,
    ) -> anyhow::Result<(&'a ManifestStaticTokenScheme, &'a AuthInject)> {
        let auth_block = manifest.auth.as_ref().ok_or_else(|| {
            anyhow!(
                "provider `{}` has no auth block; cannot run static-token init",
                manifest.id
            )
        })?;
        let wasm_manifest = auth_block.wasm_auth_manifest();
        let scheme_key = AuthManifestView::new(Some(&wasm_manifest))
            .static_token_scheme_key(self.scheme.as_deref(), None)?;
        let scheme = auth_block
            .schemes
            .get(&scheme_key)
            .ok_or_else(|| anyhow!("provider `{}` has no scheme `{scheme_key}`", manifest.id))?;
        match scheme {
            ManifestAuthScheme::StaticToken(static_token) => Ok((static_token, &auth_block.inject)),
            ManifestAuthScheme::Oauth(_) => anyhow::bail!(
                "provider `{}` scheme `{scheme_key}` is OAuth, not static-token",
                manifest.id
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MountAuth {
    config: Resolved,
    manifest: Option<AuthManifest>,
}

impl ProviderCatalog {
    pub(crate) fn load_mount_auth(&self, mount: &str) -> anyhow::Result<MountAuth> {
        let config = self.load_mount_auth_config(mount)?;
        self.resolve_mount_auth(config)
    }

    pub(crate) fn load_mount_auth_tolerating_manifest_errors(
        &self,
        mount: &str,
    ) -> anyhow::Result<MountAuth> {
        let config = self.load_mount_auth_config(mount)?;
        Ok(self.resolve_mount_auth_tolerating_manifest_errors(config))
    }

    pub(crate) fn load_all_mount_auth_tolerating_manifest_errors(
        &self,
    ) -> anyhow::Result<Vec<MountAuth>> {
        let mut mounts = self
            .session_mount_configs()?
            .into_iter()
            .map(|mount| {
                let mount = self.resolve_mount_spec(mount.config, true)?;
                Ok(self.resolve_mount_auth_tolerating_manifest_errors(mount))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        mounts.sort_by(|a, b| a.config.mount.cmp(&b.config.mount));
        Ok(mounts)
    }

    fn load_mount_auth_config(&self, mount: &str) -> anyhow::Result<Resolved> {
        let name = MountName::new(mount.to_owned())
            .with_context(|| format!("invalid mount name `{mount}`"))?;
        self.load_mount_by_name(&name)
            .with_context(|| format!("load mount config `{mount}`"))
    }

    pub(crate) fn resolve_mount_auth(&self, config: Resolved) -> anyhow::Result<MountAuth> {
        let manifest = self.auth_manifest_for(&config)?;
        Ok(MountAuth { config, manifest })
    }

    pub(crate) fn resolve_mount_auth_tolerating_manifest_errors(
        &self,
        config: Resolved,
    ) -> MountAuth {
        let manifest = self.auth_manifest_for(&config).ok().flatten();
        MountAuth { config, manifest }
    }
}

impl MountAuth {
    pub(crate) fn config(&self) -> &Resolved {
        &self.config
    }

    pub(crate) fn auth_manifest(&self) -> Option<&AuthManifest> {
        self.manifest.as_ref()
    }

    pub(crate) fn oauth_request(
        &self,
        account: Option<&str>,
        scopes: &[String],
    ) -> anyhow::Result<(OAuthRequest, CredentialTarget)> {
        let auth = self.primary_auth();
        let scheme = self
            .manifest_view()
            .oauth_scheme(auth.and_then(Auth::scheme))?
            .clone();
        let target = self.target_for_scheme(auth, &scheme.key, account)?;
        let mut request = oauth_request_from_config(auth.and_then(Auth::as_oauth), scheme)?;
        if !scopes.is_empty() {
            request.override_default_scopes(scopes.to_vec());
        }
        Ok((request, target))
    }

    pub(crate) fn static_token_target(
        &self,
        requested_scheme: Option<&str>,
        account: Option<&str>,
    ) -> anyhow::Result<CredentialTarget> {
        let auth = self.primary_auth();
        let scheme = self
            .manifest_view()
            .static_token_scheme_key(requested_scheme, auth.and_then(Auth::scheme))?;
        self.target_for_scheme(auth, &scheme, account)
    }

    pub(crate) fn status_entry(
        &self,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<Option<CredentialEntry>> {
        self.status_target()?.lookup(store)
    }

    pub(crate) fn configured_target(
        &self,
        auth: &Auth,
        account: Option<&str>,
    ) -> anyhow::Result<CredentialTarget> {
        let scheme = auth.scheme().ok_or_else(|| {
            anyhow!(
                "auth config for mount `{}` must set `scheme`",
                self.config.mount
            )
        })?;
        self.target_for_scheme(Some(auth), scheme, account)
    }

    fn primary_auth(&self) -> Option<&Auth> {
        self.config
            .auth
            .iter()
            .find(|auth| auth.is_oauth())
            .or_else(|| self.config.auth.first())
    }

    fn status_target(&self) -> anyhow::Result<CredentialTarget> {
        let auth = self.primary_auth();
        let view = self.manifest_view();
        let scheme = view
            .oauth_scheme(auth.and_then(Auth::scheme))
            .ok()
            .map(|scheme| scheme.key.clone())
            .or_else(|| {
                view.static_token_scheme_key(None, auth.and_then(Auth::scheme))
                    .ok()
            })
            .unwrap_or_else(|| "unknown".to_owned());
        self.target_for_scheme(auth, &scheme, None)
    }

    fn target_for_scheme(
        &self,
        auth: Option<&Auth>,
        scheme: &str,
        account: Option<&str>,
    ) -> anyhow::Result<CredentialTarget> {
        CredentialTarget::for_scheme(&self.config, auth, scheme, account)
            .map_err(anyhow::Error::from)
    }

    fn manifest_view(&self) -> AuthManifestView<'_> {
        AuthManifestView::new(self.manifest.as_ref())
    }
}
