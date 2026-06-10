//! Parsed index of embedded built-in provider manifests.

use crate::{AuthManifest, ProviderManifest};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use super::{Error, Resolved, Spec};

const BUILTIN_PROVIDER_MANIFESTS: &[&str] =
    include!(concat!(env!("OUT_DIR"), "/builtin_provider_manifests.rs"));

#[derive(Debug)]
pub(crate) struct Builtins {
    manifests: Vec<ProviderManifest>,
    by_id: BTreeMap<String, usize>,
    by_provider_file: BTreeMap<String, usize>,
}

static EMBEDDED: OnceLock<Result<Builtins, String>> = OnceLock::new();

impl Builtins {
    pub(crate) fn embedded() -> Result<&'static Self, Error> {
        EMBEDDED
            .get_or_init(Builtins::load)
            .as_ref()
            .map_err(|error| Error::BuiltinManifest(error.clone()))
    }

    fn load() -> Result<Self, String> {
        let mut manifests = Vec::with_capacity(BUILTIN_PROVIDER_MANIFESTS.len());
        let mut by_id = BTreeMap::new();
        let mut by_provider_file = BTreeMap::new();

        for manifest_json in BUILTIN_PROVIDER_MANIFESTS {
            let manifest = ProviderManifest::from_bytes(manifest_json.as_bytes())
                .map_err(|error| format!("parse built-in provider manifest: {error}"))?;
            if by_id.insert(manifest.id.clone(), manifests.len()).is_some() {
                return Err(format!(
                    "duplicate built-in provider manifest id `{}`",
                    manifest.id
                ));
            }
            if by_provider_file
                .insert(manifest.provider.clone(), manifests.len())
                .is_some()
            {
                return Err(format!(
                    "duplicate built-in provider manifest file `{}`",
                    manifest.provider
                ));
            }
            manifests.push(manifest);
        }

        Ok(Self {
            manifests,
            by_id,
            by_provider_file,
        })
    }

    pub(crate) fn by_id(&self, id: &str) -> Option<&ProviderManifest> {
        self.by_id.get(id).map(|index| &self.manifests[*index])
    }

    pub(crate) fn by_provider_file(&self, file_name: &str) -> Option<&ProviderManifest> {
        self.by_provider_file
            .get(file_name)
            .map(|index| &self.manifests[*index])
    }

    pub(crate) fn all_manifests(&self) -> &[ProviderManifest] {
        &self.manifests
    }

    fn manifest_for_spec(&self, config: &Spec) -> Option<&ProviderManifest> {
        if let Some(provider_id) = config.provider_id()
            && let Some(manifest) = self.by_id(provider_id)
        {
            return Some(manifest);
        }
        provider_file_name(&config.provider).and_then(|file_name| self.by_provider_file(file_name))
    }

    fn manifest_for_resolved(&self, config: &Resolved) -> Option<&ProviderManifest> {
        self.by_id(config.provider_id()).or_else(|| {
            provider_file_name(&config.provider)
                .and_then(|file_name| self.by_provider_file(file_name))
        })
    }

    pub(crate) fn apply_metadata_to(&self, config: &mut Spec) -> Result<bool, Error> {
        let Some(manifest) = self.manifest_for_spec(config) else {
            return Ok(false);
        };
        config
            .apply_provider_metadata(manifest)
            .map_err(|source| Error::ApplyBuiltinMetadata {
                provider_id: manifest.id.clone(),
                source,
            })?;
        Ok(true)
    }

    pub(crate) fn auth_manifest_for(&self, config: &Resolved) -> Option<AuthManifest> {
        self.manifest_for_resolved(config)
            .and_then(ProviderManifest::wasm_auth_manifest)
    }
}

fn provider_file_name(provider: &str) -> Option<&str> {
    Path::new(provider)
        .file_name()
        .and_then(|name| name.to_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_by_manifest_id() {
        let index = Builtins::embedded().unwrap();
        let github = index.by_id("github").expect("github manifest");
        assert_eq!(github.provider, "omnifs_provider_github.wasm");
    }

    #[test]
    fn embedded_index_excludes_fixture_provider() {
        let index = Builtins::embedded().unwrap();
        assert!(index.by_id("test-provider").is_none());
        assert!(index.by_provider_file("test_provider.wasm").is_none());
    }

    #[test]
    fn apply_metadata_to_uses_provider_file_when_id_missing() {
        let index = Builtins::embedded().unwrap();
        let mut config = Spec::parse(
            r#"{
                "provider": "omnifs_provider_github.wasm",
                "mount": "github"
            }"#,
        )
        .unwrap();
        assert!(index.apply_metadata_to(&mut config).unwrap());
        assert_eq!(config.provider_id(), Some("github"));
    }

    #[test]
    fn auth_manifest_for_returns_builtin_github_auth() {
        let index = Builtins::embedded().unwrap();
        let config = Spec::parse(
            r#"{
                "provider": "omnifs_provider_github.wasm",
                "mount": "github"
            }"#,
        )
        .unwrap();
        let resolved = config
            .into_resolved("github".to_owned(), None)
            .expect("resolved mount");
        let auth = index.auth_manifest_for(&resolved);
        assert!(
            auth.is_some(),
            "github built-in manifest should expose auth"
        );
    }
}
