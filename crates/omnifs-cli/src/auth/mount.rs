//! Mount-scoped auth loading, init auth selection, and OAuth request construction.

use anyhow::anyhow;
use omnifs_auth::{AuthManifest, AuthScheme, StaticTokenScheme};
use omnifs_provider::ProviderManifest;

use super::manifest_view::static_token_scheme_key;
use super::{Auth, OAuth, StaticToken};

impl Auth {
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
        let scheme_key = static_token_scheme_key(&wasm_manifest, self.scheme())?;
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
