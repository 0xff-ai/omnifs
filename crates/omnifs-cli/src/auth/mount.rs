//! Mount-scoped auth loading, init auth selection, and OAuth request construction.

use anyhow::{Context, anyhow};
use omnifs_auth::OAuthRequest;
use omnifs_auth::oauth_request_from_config;
use omnifs_core::AuthKind;
use omnifs_creds::CredentialStore;
use omnifs_mount::Auth;
use omnifs_mount::mounts::Resolved;
use omnifs_provider::{
    AuthInject, AuthManifest, AuthScheme, Catalog, ProviderManifest, StaticTokenScheme,
};

use super::manifest_view::AuthManifestView;
use super::readiness::AuthReadiness;
use crate::credential_target::CredentialTarget;
use crate::session::MountConfig;
use omnifs_core::MountName;

/// Auth mode chosen during `omnifs init` before a mount config exists on disk.
#[derive(Clone, Debug)]
pub(crate) struct AuthSelection {
    pub(crate) auth_type: AuthKind,
    pub(crate) scheme: Option<String>,
    pub(crate) account: Option<String>,
}

impl AuthSelection {
    pub(crate) fn from_provider_default(manifest: &ProviderManifest) -> Option<Self> {
        let auth = manifest.auth.as_ref()?;
        let (key, scheme) = auth.default_scheme()?;
        let auth_type = match scheme {
            AuthScheme::StaticToken(_) => AuthKind::StaticToken,
            _ => AuthKind::OAuth,
        };
        Some(Self {
            auth_type,
            scheme: Some(key.to_owned()),
            account: None,
        })
    }

    pub(crate) fn is_oauth(&self) -> bool {
        self.auth_type == AuthKind::OAuth
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
                auth_type: AuthKind::StaticToken,
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
    ) -> anyhow::Result<(&'a StaticTokenScheme, &'a AuthInject)> {
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
            AuthScheme::StaticToken(static_token) => Ok((static_token, &auth_block.inject)),
            _ => anyhow::bail!(
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

pub(crate) fn load_mount_auth(
    catalog: &Catalog,
    mounts: &[MountConfig],
    mount: &str,
) -> anyhow::Result<MountAuth> {
    let config = load_mount_auth_config(catalog, mounts, mount)?;
    Ok(mount_auth(catalog, config))
}

pub(crate) fn load_all_mount_auth(
    catalog: &Catalog,
    mounts: Vec<MountConfig>,
) -> anyhow::Result<Vec<MountAuth>> {
    let mut results = mounts
        .into_iter()
        .map(|m| {
            let resolved = crate::catalog::resolve_mount_spec(catalog, &m.config, true)
                .with_context(|| format!("load mount config `{}`", m.name))?;
            Ok(mount_auth(catalog, resolved))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    results.sort_by(|a, b| a.config.spec.mount.cmp(&b.config.spec.mount));
    Ok(results)
}

fn load_mount_auth_config(
    catalog: &Catalog,
    mounts: &[MountConfig],
    mount: &str,
) -> anyhow::Result<Resolved> {
    let name = MountName::new(mount.to_owned())
        .with_context(|| format!("invalid mount name `{mount}`"))?;
    crate::mount_report::load_mount_by_name(catalog, mounts, &name)
        .with_context(|| format!("load mount config `{mount}`"))
}

/// Build the auth view for an already-resolved mount. The provider's auth
/// manifest is best-effort: a missing or unreadable artifact leaves `manifest`
/// as `None`, and the auth helpers surface a clear error at the point a scheme is
/// actually needed (login, import) rather than failing the whole listing here.
pub(crate) fn mount_auth(catalog: &Catalog, config: Resolved) -> MountAuth {
    let manifest = omnifs_mount::mounts::auth_manifest_for(catalog, &config)
        .ok()
        .flatten();
    MountAuth { config, manifest }
}

impl MountAuth {
    pub(crate) fn config(&self) -> &Resolved {
        &self.config
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

    pub(crate) fn readiness(&self, store: &dyn CredentialStore) -> AuthReadiness {
        let target = match self.status_target() {
            Ok(target) => target,
            Err(error) => {
                return AuthReadiness::Error {
                    message: error.to_string(),
                };
            },
        };
        AuthReadiness::from_target(&self.config.spec.mount, target, store)
    }

    pub(crate) fn configured_target(
        &self,
        auth: &Auth,
        account: Option<&str>,
    ) -> anyhow::Result<CredentialTarget> {
        let scheme = auth.scheme().ok_or_else(|| {
            anyhow!(
                "auth config for mount `{}` must set `scheme`",
                self.config.spec.mount
            )
        })?;
        self.target_for_scheme(Some(auth), scheme, account)
    }

    fn primary_auth(&self) -> Option<&Auth> {
        self.config
            .spec
            .auth
            .iter()
            .find(|auth| auth.is_oauth())
            .or_else(|| self.config.spec.auth.first())
    }

    fn status_target(&self) -> anyhow::Result<CredentialTarget> {
        let Some(auth) = self.primary_auth() else {
            return Ok(CredentialTarget::None);
        };
        let view = self.manifest_view();
        let scheme = if auth.is_oauth() {
            view.oauth_scheme(auth.scheme())
                .map(|scheme| scheme.key.clone())
                .or_else(|_| {
                    auth.scheme()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow!("missing auth.scheme"))
                })?
        } else {
            view.static_token_scheme_key(None, auth.scheme())
                .or_else(|_| {
                    auth.scheme()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow!("missing auth.scheme"))
                })?
        };
        CredentialTarget::for_configured_auth(&self.config, auth, Some(&scheme), auth.account())
    }

    fn target_for_scheme(
        &self,
        auth: Option<&Auth>,
        scheme: &str,
        account: Option<&str>,
    ) -> anyhow::Result<CredentialTarget> {
        CredentialTarget::for_scheme(&self.config, auth, scheme, account)
    }

    fn manifest_view(&self) -> AuthManifestView<'_> {
        AuthManifestView::new(self.manifest.as_ref())
    }
}
