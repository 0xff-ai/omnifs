//! Mount-scoped auth loading, init auth selection, and OAuth request construction.

use anyhow::anyhow;
use omnifs_auth::{AuthManifest, AuthScheme, StaticTokenScheme};
use omnifs_provider::ProviderManifest;

use super::manifest_view::AuthManifestView;
use super::{Auth, OAuth, StaticToken};

impl Auth {
    pub(crate) fn from_provider_default(manifest: &ProviderManifest) -> Option<Auth> {
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

    pub(crate) fn from_scheme(
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

    pub(crate) fn static_token(
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

    pub(crate) fn promote_to_static(
        self,
        auth_manifest: Option<&AuthManifest>,
        provider_id: &str,
    ) -> anyhow::Result<Auth> {
        if !self.is_oauth() {
            return Ok(self);
        }
        let account = self.account().map(str::to_owned);
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
        &self,
        manifest: &'a ProviderManifest,
    ) -> anyhow::Result<&'a StaticTokenScheme> {
        let auth_block = manifest.auth.as_ref().ok_or_else(|| {
            anyhow!(
                "provider `{}` has no auth block; cannot run static-token init",
                manifest.id
            )
        })?;
        let wasm_manifest = auth_block.wasm_auth_manifest();
        let scheme_key = AuthManifestView::new(Some(&wasm_manifest))
            .static_token_scheme_key(self.scheme(), None)?;
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
}
