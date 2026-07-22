//! Mount-scoped auth loading, init auth selection, and OAuth request construction.

use anyhow::{Context, anyhow};
use omnifs_auth::OAuthRequest;
use omnifs_workspace::authn::{
    AccountId, AuthManifest, AuthScheme, CredentialId, StaticTokenScheme,
};
use omnifs_workspace::creds::CredentialStore;
use omnifs_workspace::mounts::{Auth, Name as MountName, OAuth, Registry, Spec, StaticToken};
use omnifs_workspace::provider::{Catalog, ProviderAuthManifest, ProviderManifest};

use super::manifest_view::AuthManifestView;
use super::readiness::AuthReadiness;

pub(crate) fn auth_from_provider_default(manifest: &ProviderManifest) -> Option<Auth> {
    let (scheme, default) = manifest.auth.as_ref()?.default_scheme()?;
    let scheme = Some(scheme.to_owned());
    match default {
        AuthScheme::None => None,
        AuthScheme::StaticToken(_) => Some(Auth::StaticToken(StaticToken {
            scheme,
            account: None,
        })),
        AuthScheme::Oauth(_) => Some(Auth::OAuth(OAuth {
            scheme,
            ..OAuth::default()
        })),
    }
}

pub(crate) fn auth_from_scheme(
    auth_manifest: Option<&AuthManifest>,
    scheme: &str,
    account: Option<String>,
) -> anyhow::Result<Auth> {
    let manifest = auth_manifest.ok_or_else(|| anyhow!("provider has no auth manifest"))?;
    if manifest.resolve_static_scheme(Some(scheme)).is_ok() {
        return Ok(Auth::StaticToken(StaticToken {
            scheme: Some(scheme.to_owned()),
            account,
        }));
    }
    if manifest.resolve_oauth_scheme(Some(scheme)).is_ok() {
        return Ok(Auth::OAuth(OAuth {
            scheme: Some(scheme.to_owned()),
            account,
            ..OAuth::default()
        }));
    }
    anyhow::bail!("provider has no auth scheme `{scheme}`")
}

pub(crate) fn static_token_auth(
    auth_manifest: Option<&AuthManifest>,
    scheme: Option<&str>,
    account: Option<String>,
) -> anyhow::Result<Auth> {
    let manifest = auth_manifest.ok_or_else(|| anyhow!("provider has no auth manifest"))?;
    let static_scheme = manifest.resolve_static_scheme(scheme)?;
    Ok(Auth::StaticToken(StaticToken {
        scheme: Some(static_scheme.key.clone()),
        account,
    }))
}

pub(crate) fn promote_imported_static(
    auth: Auth,
    auth_manifest: Option<&AuthManifest>,
    provider_id: &str,
) -> anyhow::Result<Auth> {
    if !auth.is_oauth() {
        return Ok(auth);
    }
    let account = auth.account().map(str::to_owned);
    match AuthManifestView::new(auth_manifest).first_static_token_scheme_key() {
        Some(scheme) => Ok(Auth::StaticToken(StaticToken {
            scheme: Some(scheme),
            account,
        })),
        None => anyhow::bail!(
            "imported a static token for `{provider_id}`, but the provider declares no static-token scheme; remove the ambient credential or run OAuth"
        ),
    }
}

pub(crate) fn static_token_scheme<'a>(
    auth: &Auth,
    manifest: &'a ProviderManifest,
) -> anyhow::Result<&'a StaticTokenScheme> {
    let auth_block = manifest.auth.as_ref().ok_or_else(|| {
        anyhow!(
            "provider `{}` has no auth block; cannot run static-token init",
            manifest.id
        )
    })?;
    let wasm_manifest = auth_block.wasm_auth_manifest();
    let scheme_key =
        AuthManifestView::new(Some(&wasm_manifest)).static_token_scheme_key(auth.scheme(), None)?;
    let scheme = auth_block
        .scheme(&scheme_key)
        .ok_or_else(|| anyhow!("provider `{}` has no scheme `{scheme_key}`", manifest.id))?;
    match scheme {
        AuthScheme::StaticToken(static_token) => Ok(static_token),
        _ => anyhow::bail!(
            "provider `{}` scheme `{scheme_key}` is OAuth, not static-token",
            manifest.id
        ),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MountAuth {
    spec: Spec,
    manifest: Option<AuthManifest>,
}

impl MountAuth {
    pub(crate) fn load(catalog: &Catalog, mounts: &Registry, mount: &str) -> anyhow::Result<Self> {
        let name = MountName::new(mount.to_owned())
            .with_context(|| format!("invalid mount name `{mount}`"))?;
        let spec = mounts
            .get(&name)
            .cloned()
            .ok_or_else(|| anyhow!("no mount config named `{name}`"))
            .with_context(|| format!("load mount config `{mount}`"))?;
        Ok(Self::from_spec(catalog, spec))
    }

    /// Build the auth view for a mount spec. The provider's auth manifest is
    /// best-effort: a missing or unreadable artifact leaves `manifest` as
    /// `None`, and the auth helpers surface a clear error at the point a scheme
    /// is actually needed (login, import) rather than failing the whole listing
    /// here. The scheme name itself is already baked into `spec.auth` at
    /// creation, so it is available even when the manifest is not.
    pub(crate) fn from_spec(catalog: &Catalog, spec: Spec) -> Self {
        let manifest = omnifs_workspace::mounts::pinned_manifest(catalog, &spec)
            .ok()
            .flatten()
            .and_then(|manifest| {
                manifest
                    .auth
                    .as_ref()
                    .map(ProviderAuthManifest::wasm_auth_manifest)
            });
        Self { spec, manifest }
    }

    pub(crate) fn spec(&self) -> &Spec {
        &self.spec
    }

    pub(crate) fn oauth_request(
        &self,
        account: Option<&str>,
        scopes: &[String],
    ) -> anyhow::Result<(OAuthRequest, CredentialId)> {
        let auth = self.primary_auth();
        let scheme = self
            .manifest_view()
            .oauth_scheme(auth.and_then(Auth::scheme))?
            .clone();
        let target = self.target_for_scheme(auth, &scheme.key, account)?;
        let mut request = OAuthRequest::from_mount_config(auth.and_then(Auth::as_oauth), scheme)?;
        if !scopes.is_empty() {
            request.override_default_scopes(scopes.to_vec());
        }
        Ok((request, target))
    }

    pub(crate) fn readiness(&self, store: &dyn CredentialStore) -> AuthReadiness {
        let target = match self.credential_id() {
            Ok(target) => target,
            Err(error) => {
                return AuthReadiness::Error {
                    message: error.to_string(),
                };
            },
        };
        AuthReadiness::from_credential(&self.spec.mount, target.as_ref(), store)
    }

    pub(crate) fn configured_credential_id(&self, auth: &Auth) -> anyhow::Result<CredentialId> {
        let scheme = auth.scheme().ok_or_else(|| {
            anyhow!(
                "auth config for mount `{}` must set `scheme`",
                self.spec.mount
            )
        })?;
        self.target_for_scheme(Some(auth), scheme, None)
    }

    /// Resolve the credential identity selected by this mount's persisted auth
    /// block. The manifest remains best-effort because specs carry
    /// their selected scheme.
    pub(crate) fn credential_id(&self) -> anyhow::Result<Option<CredentialId>> {
        let Some(auth) = self.primary_auth() else {
            return Ok(None);
        };
        let scheme = self.manifest_view().configured_scheme_key(auth)?;
        self.target_for_scheme(Some(auth), &scheme, None).map(Some)
    }

    fn primary_auth(&self) -> Option<&Auth> {
        self.spec.auth.as_ref()
    }

    fn target_for_scheme(
        &self,
        auth: Option<&Auth>,
        scheme: &str,
        account: Option<&str>,
    ) -> anyhow::Result<CredentialId> {
        match (account, auth) {
            (Some(account), _) => {
                CredentialId::new(self.spec.provider_name().as_str(), scheme, account)
                    .map_err(Into::into)
            },
            (None, Some(auth)) => {
                CredentialId::for_mount(self.spec.provider_name(), auth, scheme).map_err(Into::into)
            },
            (None, None) => CredentialId::new(
                self.spec.provider_name().as_str(),
                scheme,
                AccountId::default_account().as_str(),
            )
            .map_err(Into::into),
        }
    }

    fn manifest_view(&self) -> AuthManifestView<'_> {
        AuthManifestView::new(self.manifest.as_ref())
    }
}
