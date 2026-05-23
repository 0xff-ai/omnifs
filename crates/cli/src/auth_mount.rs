//! Mount-scoped auth loading, init auth selection, and OAuth request construction.

use anyhow::{Context, anyhow};
use omnifs_auth::OAuthRequest;
use omnifs_host::auth::oauth_request_from_config;
use omnifs_host::config::{AuthConfig, EffectiveConfig};
use omnifs_mount_schema::{
    AuthInject, AuthManifest, ManifestAuthScheme, ManifestStaticTokenScheme, ProviderManifest,
};

use crate::auth_manifest_view::AuthManifestView;
use crate::catalog::ProviderCatalog;
use crate::credential_target::CredentialTarget;
use omnifs_model::MountName;

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

pub(crate) struct MountAuth {
    pub(crate) config: EffectiveConfig,
}

impl MountAuth {
    pub(crate) fn load(catalog: &ProviderCatalog, mount: &str) -> anyhow::Result<Self> {
        let name = MountName::new(mount.to_owned())
            .with_context(|| format!("invalid mount name `{mount}`"))?;
        let loaded = catalog
            .load_mount_by_name(&name)
            .with_context(|| format!("load mount config `{mount}`"))?;
        Ok(Self {
            config: loaded.config,
        })
    }

    pub(crate) fn oauth_request(
        &self,
        catalog: &ProviderCatalog,
        account: Option<&str>,
        scopes: &[String],
    ) -> anyhow::Result<(OAuthRequest, CredentialTarget)> {
        let auth = primary_auth(&self.config);
        let manifest = catalog
            .auth_manifest_for(&self.config)?
            .ok_or_else(|| anyhow!("provider has no auth manifest"))?;
        let scheme = AuthManifestView::new(Some(&manifest))
            .oauth_scheme(auth.and_then(AuthConfig::scheme))?
            .clone();
        let target = CredentialTarget::for_scheme(&self.config, auth, &scheme.key, account)?;
        let mut request = oauth_request_from_config(auth.and_then(AuthConfig::as_oauth), scheme)?;
        if !scopes.is_empty() {
            request.override_default_scopes(scopes.to_vec());
        }
        Ok((request, target))
    }
}

fn primary_auth(config: &EffectiveConfig) -> Option<&AuthConfig> {
    config
        .auth
        .iter()
        .find(|auth| auth.is_oauth())
        .or_else(|| config.auth.first())
}
