//! Provider WASM installation into the content-addressed store.
//!
//! Each provider WASM is hashed to its [`ProviderId`], written under
//! `providers_dir/<hex>.wasm`, and recorded in `index.json` (advancing
//! `latest[name]`). Content addressing makes installation idempotent.

use anyhow::Context as _;
use std::io::{Cursor, Read};
use std::path::Path;

use omnifs_provider::{Artifact, ProviderStore};

static EMBEDDED_PROVIDER_BUNDLE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/provider-bundle.tar.zst"));

/// Install the launcher's embedded provider bundle into the content-addressed
/// store at `providers_dir`. Idempotent: content-addressed artifacts already
/// present are skipped, so a warm launch re-runs cheaply.
pub(crate) fn ensure_providers_installed(providers_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(providers_dir)
        .with_context(|| format!("create {}", providers_dir.display()))?;
    let store = ProviderStore::new(providers_dir);

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
        let artifact = Artifact::from_bytes(name.clone(), bytes)
            .with_context(|| format!("read provider metadata from `{name}`"))?;
        store
            .add_artifact(artifact)
            .with_context(|| format!("install embedded provider `{name}`"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_providers_installed_populates_store() {
        let providers_dir = tempfile::tempdir().expect("temp providers dir");

        ensure_providers_installed(providers_dir.path()).expect("install providers");
        // Second call is a content-addressed no-op success.
        ensure_providers_installed(providers_dir.path()).expect("reinstall providers");

        let store = ProviderStore::new(providers_dir.path());
        let index = store.read_index().expect("read index");
        assert!(
            !index.providers.is_empty(),
            "expected at least one installed provider"
        );
        for entry in &index.providers {
            assert!(
                store.artifact_path(&entry.id).is_file(),
                "missing retained artifact for `{}`",
                entry.name
            );
            assert_ne!(
                entry.name.as_str(),
                "test-provider",
                "the test fixture provider must not ship in the embedded bundle"
            );
        }
    }
}
