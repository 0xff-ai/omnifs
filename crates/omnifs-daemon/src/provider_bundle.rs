//! The provider bundle compiled into the daemon binary.

use anyhow::Context as _;
use omnifs_api::{ProviderMetadata, ProviderReference};
use omnifs_provider::{Artifact, ProviderManifest};
use std::io::{Cursor, Read};

static EMBEDDED_PROVIDER_BUNDLE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/provider-bundle.tar.zst"));

#[derive(Debug, Default)]
pub(crate) struct EmbeddedProviders {
    entries: Vec<EmbeddedProvider>,
}

#[derive(Debug)]
pub(crate) struct EmbeddedProvider {
    artifact: Artifact,
    manifest: ProviderManifest,
}

impl EmbeddedProviders {
    pub(crate) fn load() -> anyhow::Result<Self> {
        let mut entries = Vec::new();
        let decoder = zstd::stream::read::Decoder::new(Cursor::new(EMBEDDED_PROVIDER_BUNDLE))
            .context("decode embedded provider bundle")?;
        let mut archive = tar::Archive::new(decoder);
        for entry in archive.entries().context("read embedded provider bundle")? {
            let mut entry = entry.context("read embedded provider bundle entry")?;
            let name = entry
                .path()
                .context("read embedded provider bundle path")?
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .context("embedded provider bundle entry has no file name")?;
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .with_context(|| format!("read embedded provider bundle file `{name}`"))?;
            let (artifact, manifest) = Artifact::from_bytes_with_manifest(name.clone(), bytes)
                .with_context(|| format!("validate embedded provider artifact `{name}`"))?;
            entries.push(EmbeddedProvider { artifact, manifest });
        }
        // Sorted for deterministic `metadata()` output (the order daemon
        // control-plane responses list embedded providers in), not to serve
        // `by_name` below. `build.rs` selects at most one artifact per
        // provider name from the store's index before this ever runs, so
        // `entries` holds only a handful of distinct names and a linear
        // scan over them needs no binary search.
        entries.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
        Ok(Self { entries })
    }

    pub(crate) fn by_name(&self, name: &str) -> Option<&EmbeddedProvider> {
        self.entries.iter().find(|entry| entry.manifest.id == name)
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &EmbeddedProvider> {
        self.entries.iter()
    }

    pub(crate) fn metadata(&self) -> Vec<ProviderMetadata> {
        self.entries
            .iter()
            .map(|entry| ProviderMetadata {
                reference: ProviderReference {
                    id: entry.artifact.id(),
                    name: entry.manifest.id.clone(),
                    version: entry.manifest.version.clone(),
                },
                manifest: serde_json::to_vec(&entry.manifest)
                    .expect("embedded provider manifest serializes"),
            })
            .collect()
    }
}

impl EmbeddedProvider {
    pub(crate) fn artifact(&self) -> &Artifact {
        &self.artifact
    }

    pub(crate) fn catalog_name(&self) -> &str {
        &self.manifest.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_contains_validated_product_providers_only() {
        let embedded = EmbeddedProviders::load().expect("load embedded providers");
        assert!(!embedded.entries.is_empty());
        assert!(
            embedded
                .entries
                .iter()
                .all(|entry| entry.manifest.id != "test-provider")
        );
    }
}
