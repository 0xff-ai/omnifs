//! Provider WASM installation into the content-addressed store.
//!
//! Each provider WASM is hashed to its [`ProviderId`], written under
//! `providers_dir/by-hash/<hex>.wasm`, and recorded in `index.json` (advancing
//! `latest[name]`). The host-internal archive tool is written flat, never hashed
//! or indexed. Content addressing makes installation idempotent: an artifact
//! already present under `by-hash/` is skipped.

use anyhow::Context as _;
use std::io::{Cursor, Read};
use std::path::Path;
use std::process::Command;

use omnifs_core::{ProviderId, ProviderMeta, ProviderName, ProviderVersion};
use omnifs_mount::mounts::ProviderStore;

const ARCHIVE_TOOL_WASM: &str = "omnifs_tool_archive.wasm";

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
        install_artifact(&store, providers_dir, &name, &bytes)?;
    }
    Ok(())
}

/// Export provider WASM freshly from the workspace via Docker, then ingest the
/// exported artifacts into the content-addressed store. Used by `omnifs dev`
/// build mode, which rebuilds providers from the source checkout.
pub(crate) fn install_workspace_bundle(
    workspace: &Path,
    providers_dir: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(providers_dir)
        .with_context(|| format!("create {}", providers_dir.display()))?;
    anstream::println!(
        "Exporting provider WASM bundle into {}",
        providers_dir.display()
    );
    let output = format!("type=local,dest={}", providers_dir.display());
    let status = Command::new("docker")
        .args([
            "build",
            "--target",
            "wasm-artifacts",
            "--output",
            &output,
            ".",
        ])
        .current_dir(workspace)
        .status()
        .context("invoke docker build for provider WASM artifacts")?;
    if !status.success() {
        anyhow::bail!("provider WASM export failed");
    }
    ingest_exported_artifacts(providers_dir)
}

/// Ingest the flat WASM files Docker exported into `providers_dir` into the
/// content-addressed store. The archive tool stays flat; every other WASM is
/// hashed and indexed.
fn ingest_exported_artifacts(providers_dir: &Path) -> anyhow::Result<()> {
    let store = ProviderStore::new(providers_dir);
    let read = std::fs::read_dir(providers_dir)
        .with_context(|| format!("scan {}", providers_dir.display()))?;
    for entry in read {
        let path = entry
            .with_context(|| format!("scan {}", providers_dir.display()))?
            .path();
        if path.extension().is_none_or(|ext| ext != "wasm") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == ARCHIVE_TOOL_WASM {
            continue;
        }
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        install_artifact(&store, providers_dir, name, &bytes)?;
    }
    Ok(())
}

/// Install one bundle entry. The archive tool is written flat; a provider is
/// hashed into `by-hash/` and recorded in the index with the name and version
/// from its embedded manifest.
fn install_artifact(
    store: &ProviderStore,
    providers_dir: &Path,
    name: &str,
    bytes: &[u8],
) -> anyhow::Result<()> {
    if name == ARCHIVE_TOOL_WASM {
        let target = providers_dir.join(name);
        if target.is_file() && std::fs::read(&target).is_ok_and(|existing| existing == bytes) {
            return Ok(());
        }
        let mut temp = target.clone();
        temp.set_file_name(format!("{name}.tmp-{}", std::process::id()));
        std::fs::write(&temp, bytes).with_context(|| format!("write {}", temp.display()))?;
        std::fs::rename(&temp, &target).with_context(|| {
            format!(
                "move provider bundle file {} to {}",
                temp.display(),
                target.display()
            )
        })?;
        return Ok(());
    }

    let id = ProviderId::from_wasm_bytes(bytes);
    store
        .put_if_absent(&id, bytes)
        .with_context(|| format!("store provider `{name}`"))?;
    let manifest = omnifs_provider::read_provider_metadata_section(bytes)
        .with_context(|| format!("read provider manifest from `{name}`"))?
        .with_context(|| format!("provider `{name}` has no embedded manifest section"))?;
    let provider_name = ProviderName::new(manifest.id.clone()).with_context(|| {
        format!(
            "provider `{name}` has an invalid manifest id `{}`",
            manifest.id
        )
    })?;
    let meta = ProviderMeta {
        name: provider_name,
        version: manifest.version.clone().map(ProviderVersion::new),
    };
    store
        .install(id, meta, name.to_string())
        .with_context(|| format!("index provider `{name}`"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_providers_installed_populates_store_and_archive_tool() {
        let providers_dir = tempfile::tempdir().expect("temp providers dir");

        ensure_providers_installed(providers_dir.path()).expect("install providers");
        // Second call is a content-addressed no-op success.
        ensure_providers_installed(providers_dir.path()).expect("reinstall providers");

        // The archive tool lives flat, never in the content-addressed store.
        assert!(
            providers_dir.path().join(ARCHIVE_TOOL_WASM).is_file(),
            "archive tool must be installed flat"
        );

        let store = ProviderStore::new(providers_dir.path());
        let index = store.read_index().expect("read index");
        assert!(
            !index.providers.is_empty(),
            "expected at least one installed provider"
        );
        for entry in &index.providers {
            assert!(
                store.by_hash_path(&entry.id).is_file(),
                "missing by-hash artifact for `{}`",
                entry.name
            );
            assert_ne!(
                entry.name.as_str(),
                "test-provider",
                "the test fixture provider must not ship in the embedded bundle"
            );
        }
        assert!(
            index
                .providers
                .iter()
                .all(|entry| entry.file != ARCHIVE_TOOL_WASM),
            "the archive tool must not be indexed"
        );
    }
}
