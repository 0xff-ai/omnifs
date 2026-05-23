//! Parsed index of embedded built-in provider manifests.

use anyhow::{Context, anyhow};
use omnifs_host::config::{EffectiveConfig, InstanceConfig};
use omnifs_mount_schema::{AuthManifest, ProviderManifest};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

const BUILTIN_PROVIDER_MANIFESTS: &[&str] =
    include!(concat!(env!("OUT_DIR"), "/builtin_provider_manifests.rs"));

#[derive(Debug)]
pub(crate) struct BuiltinManifestIndex {
    manifests: Vec<ProviderManifest>,
    by_id: BTreeMap<String, usize>,
    by_provider_file: BTreeMap<String, usize>,
}

static EMBEDDED: OnceLock<anyhow::Result<BuiltinManifestIndex>> = OnceLock::new();

impl BuiltinManifestIndex {
    pub(crate) fn embedded() -> anyhow::Result<&'static Self> {
        EMBEDDED
            .get_or_init(BuiltinManifestIndex::load)
            .as_ref()
            .map_err(|error| anyhow!(error.to_string()))
    }

    fn load() -> anyhow::Result<Self> {
        let mut manifests = Vec::with_capacity(BUILTIN_PROVIDER_MANIFESTS.len());
        let mut by_id = BTreeMap::new();
        let mut by_provider_file = BTreeMap::new();

        for manifest_json in BUILTIN_PROVIDER_MANIFESTS {
            let manifest = ProviderManifest::from_bytes(manifest_json.as_bytes())
                .context("parse built-in provider manifest")?;
            if by_id.insert(manifest.id.clone(), manifests.len()).is_some() {
                return Err(anyhow!(
                    "duplicate built-in provider manifest id `{}`",
                    manifest.id
                ));
            }
            if by_provider_file
                .insert(manifest.provider.clone(), manifests.len())
                .is_some()
            {
                return Err(anyhow!(
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

    pub(crate) fn manifest_auth_pairs(
        &self,
    ) -> impl Iterator<Item = (&ProviderManifest, Option<AuthManifest>)> + '_ {
        self.manifests.iter().map(|manifest| {
            let auth = manifest.wasm_auth_manifest();
            (manifest, auth)
        })
    }

    fn manifest_for_instance_config(&self, config: &InstanceConfig) -> Option<&ProviderManifest> {
        if let Some(provider_id) = config.provider_id()
            && let Some(manifest) = self.by_id(provider_id)
        {
            return Some(manifest);
        }
        provider_file_name(&config.provider).and_then(|file_name| self.by_provider_file(file_name))
    }

    fn manifest_for_effective_config(&self, config: &EffectiveConfig) -> Option<&ProviderManifest> {
        self.by_id(config.provider_id()).or_else(|| {
            provider_file_name(&config.provider)
                .and_then(|file_name| self.by_provider_file(file_name))
        })
    }

    pub(crate) fn apply_metadata_to(&self, config: &mut InstanceConfig) -> anyhow::Result<bool> {
        let Some(manifest) = self.manifest_for_instance_config(config) else {
            return Ok(false);
        };
        config
            .apply_provider_metadata(manifest)
            .with_context(|| format!("apply built-in provider metadata for `{}`", manifest.id))?;
        Ok(true)
    }

    pub(crate) fn auth_manifest_for(&self, config: &EffectiveConfig) -> Option<AuthManifest> {
        self.manifest_for_effective_config(config)
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
    fn embedded_index_length_matches_manifest_count() {
        let index = BuiltinManifestIndex::embedded().unwrap();
        assert_eq!(
            index.all_manifests().len(),
            BUILTIN_PROVIDER_MANIFESTS.len()
        );
    }

    #[test]
    fn lookup_by_manifest_id() {
        let index = BuiltinManifestIndex::embedded().unwrap();
        let github = index.by_id("github").expect("github manifest");
        assert_eq!(github.provider, "omnifs_provider_github.wasm");
    }

    #[test]
    fn lookup_by_provider_filename() {
        let index = BuiltinManifestIndex::embedded().unwrap();
        let github = index
            .by_provider_file("omnifs_provider_github.wasm")
            .expect("github wasm manifest");
        assert_eq!(github.id, "github");
    }

    #[test]
    fn apply_metadata_to_uses_provider_file_when_id_missing() {
        let index = BuiltinManifestIndex::embedded().unwrap();
        let mut config = InstanceConfig::parse(
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
        let index = BuiltinManifestIndex::embedded().unwrap();
        let config = InstanceConfig::parse(
            r#"{
                "provider": "omnifs_provider_github.wasm",
                "mount": "github"
            }"#,
        )
        .unwrap();
        let effective = config
            .into_effective("github".to_owned(), None)
            .expect("effective config");
        let auth = index.auth_manifest_for(&effective);
        assert!(
            auth.is_some(),
            "github built-in manifest should expose auth"
        );
    }
}
