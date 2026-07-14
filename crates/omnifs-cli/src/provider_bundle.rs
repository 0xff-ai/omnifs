//! The compiled provider bundle, parsed without retaining it wholesale.

use anyhow::Context as _;
use std::io::{Cursor, Read};

use omnifs_workspace::provider::{Artifact, ProviderManifest, ProviderWasm};

static EMBEDDED_PROVIDER_BUNDLE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/provider-bundle.tar.zst"));

#[derive(Debug)]
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
        // The bundle is a compile-time artifact this crate's build script produced
        // from the provider catalog, so its entries are trusted bare file names.
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
            let manifest = ProviderWasm::from_bytes(bytes.clone())
                .metadata()
                .with_context(|| format!("read provider metadata from `{name}`"))?
                .with_context(|| format!("provider `{name}` has no metadata"))?;
            let artifact = Artifact::from_bytes(name.clone(), bytes)
                .with_context(|| format!("validate provider artifact `{name}`"))?;
            entries.push(EmbeddedProvider { artifact, manifest });
        }
        entries.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
        Ok(Self { entries })
    }

    pub(crate) fn entries(&self) -> &[EmbeddedProvider] {
        &self.entries
    }

    pub(crate) fn by_name(&self, name: &str) -> Option<&EmbeddedProvider> {
        self.entries.iter().find(|entry| entry.manifest.id == name)
    }

    pub(crate) fn by_id(
        &self,
        id: &omnifs_workspace::ids::ProviderId,
    ) -> Option<&EmbeddedProvider> {
        self.entries.iter().find(|entry| entry.artifact.id() == *id)
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.manifest.id.as_str())
    }
}

impl EmbeddedProvider {
    pub(crate) fn artifact(&self) -> &Artifact {
        &self.artifact
    }

    pub(crate) fn manifest(&self) -> &ProviderManifest {
        &self.manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_bundle_parses_validated_entries_without_store_side_effects() {
        let embedded = EmbeddedProviders::load().expect("parse embedded providers");
        assert!(!embedded.entries().is_empty());
        assert!(!embedded.names().any(|name| name == "test-provider"));
    }
}
