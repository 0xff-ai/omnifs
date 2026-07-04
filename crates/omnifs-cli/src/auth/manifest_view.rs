use anyhow::anyhow;
use omnifs_workspace::authn::{AuthManifest, OauthScheme};

const DEFAULT_STATIC_SCHEME: &str = "static-token";

pub(crate) struct AuthManifestView<'a> {
    manifest: Option<&'a AuthManifest>,
}

impl<'a> AuthManifestView<'a> {
    pub(crate) fn new(manifest: Option<&'a AuthManifest>) -> Self {
        Self { manifest }
    }

    pub(crate) fn oauth_scheme(&self, requested: Option<&str>) -> anyhow::Result<&'a OauthScheme> {
        let manifest = self
            .manifest
            .ok_or_else(|| anyhow!("provider has no auth manifest"))?;
        manifest
            .resolve_oauth_scheme(requested)
            .map_err(anyhow::Error::from)
    }

    pub(crate) fn static_token_scheme_key(
        &self,
        requested: Option<&str>,
        mount_scheme: Option<&str>,
    ) -> anyhow::Result<String> {
        if let Some(requested) = requested {
            return Ok(requested.to_owned());
        }
        if let Some(mount_scheme) = mount_scheme
            && self.has_static_token_scheme(mount_scheme)
        {
            return Ok(mount_scheme.to_owned());
        }
        let Some(first) = self.first_static_token_scheme_key() else {
            return Ok(DEFAULT_STATIC_SCHEME.to_owned());
        };
        if self.static_token_scheme_count() > 1 {
            anyhow::bail!("multiple static-token schemes are declared; pass --scheme");
        }
        Ok(first)
    }

    pub(crate) fn first_static_token_scheme_key(&self) -> Option<String> {
        self.manifest
            .and_then(|manifest| manifest.first_static_scheme_key().map(str::to_owned))
    }

    fn has_static_token_scheme(&self, key: &str) -> bool {
        self.manifest
            .is_some_and(|manifest| manifest.resolve_static_scheme(Some(key)).is_ok())
    }

    fn static_token_scheme_count(&self) -> usize {
        self.manifest.map_or(0, AuthManifest::static_scheme_count)
    }
}
