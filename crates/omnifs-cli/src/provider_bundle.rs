//! Provider WASM installation into the content-addressed store.
//!
//! Each provider WASM is hashed to its [`ProviderId`], written under
//! `providers_dir/<hex>.wasm`, and recorded in `index.json` (advancing
//! `latest[name]`). Content addressing makes installation idempotent.

use anyhow::Context as _;
use std::collections::BTreeSet;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use omnifs_provider::{Artifact, ArtifactLoadError, ProviderStore};

const FIXTURE_PROVIDER_DIRS: &[&str] = &["test"];

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

/// Install freshly-built provider WASM from the workspace target dir into the
/// content-addressed store. Used by `omnifs dev`, which consumes host-built WASM
/// rather than the bundle embedded at CLI compile time.
pub(crate) fn install_target_providers(
    workspace: &Path,
    providers_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let artifact_dir = target_artifact_dir(workspace);
    let expected = expected_files(workspace)?;
    let missing = expected
        .iter()
        .filter(|file| !artifact_dir.join(file).is_file())
        .cloned()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        missing.is_empty(),
        "provider WASM artifacts missing in {}; run `just providers build` first (missing: {})",
        artifact_dir.display(),
        missing.join(", ")
    );

    std::fs::create_dir_all(providers_dir)
        .with_context(|| format!("create {}", providers_dir.display()))?;
    anstream::println!(
        "Installing provider WASM from {} into {}",
        artifact_dir.display(),
        providers_dir.display()
    );

    let store = ProviderStore::new(providers_dir);
    for file in &expected {
        let source = artifact_dir.join(file);
        install_file(&store, &source)?;
    }
    report_unexpected_wasm(&artifact_dir, &expected)?;
    Ok(artifact_dir)
}

#[must_use]
pub(crate) fn target_artifact_dir(workspace: &Path) -> PathBuf {
    workspace.join("target/wasm32-wasip2/release")
}

/// The provider WASM filenames `omnifs dev` expects in the workspace build
/// output: each non-fixture provider's `provider` artifact. Derived by scanning
/// the checkout's `providers/` directory, the same source the build script
/// bundles from.
fn expected_files(workspace: &Path) -> anyhow::Result<BTreeSet<String>> {
    let provider_root = workspace.join("providers");
    let mut files = BTreeSet::new();
    let read = std::fs::read_dir(&provider_root)
        .with_context(|| format!("scan {}", provider_root.display()))?;
    for entry in read {
        let entry = entry.with_context(|| format!("scan {}", provider_root.display()))?;
        let dir_name = entry.file_name();
        let Some(dir_name) = dir_name.to_str() else {
            continue;
        };
        // Provider crate `providers/<name>` (crate `omnifs-provider-<name>`)
        // builds to `omnifs_provider_<name>.wasm`. The manifest travels inside
        // the wasm custom section now, so there is no `omnifs.provider.json` to
        // read: the provider set is the crate dirs minus the test fixtures.
        if FIXTURE_PROVIDER_DIRS.contains(&dir_name) || !entry.path().join("Cargo.toml").is_file() {
            continue;
        }
        files.insert(format!(
            "omnifs_provider_{}.wasm",
            dir_name.replace('-', "_")
        ));
    }
    Ok(files)
}

fn report_unexpected_wasm(artifact_dir: &Path, expected: &BTreeSet<String>) -> anyhow::Result<()> {
    let read = std::fs::read_dir(artifact_dir)
        .with_context(|| format!("scan {}", artifact_dir.display()))?;
    for entry in read {
        let path = entry
            .with_context(|| format!("scan {}", artifact_dir.display()))?
            .path();
        if path.extension().is_none_or(|ext| ext != "wasm") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            anstream::eprintln!("  ! skipping invalid provider WASM {}", path.display());
            continue;
        };
        if expected.contains(name) {
            continue;
        }
        match Artifact::from_file(&path) {
            Ok(_) => {
                anstream::eprintln!(
                    "  ! skipping unexpected provider WASM `{name}`: not part of providers/"
                );
            },
            Err(ArtifactLoadError::Artifact(error)) => {
                anstream::eprintln!("  ! skipping invalid provider WASM `{name}`: {error}");
            },
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn install_file(store: &ProviderStore, path: &Path) -> anyhow::Result<()> {
    match Artifact::from_file(path) {
        Ok(artifact) => {
            let file = artifact.file().to_string();
            store
                .add_artifact(artifact)
                .with_context(|| format!("install provider artifact `{file}`"))?;
            Ok(())
        },
        Err(ArtifactLoadError::Artifact(error)) => Err(error).with_context(|| {
            format!(
                "provider artifact {} is invalid",
                display_provider_path(path)
            )
        }),
        Err(error) => Err(error.into()),
    }
}

fn display_provider_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| path.display().to_string(), ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_WASM: &[u8] = b"\0asm\x01\0\0\0";

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

    #[test]
    fn install_target_providers_skips_invalid_unexpected_wasm() {
        let workspace = tempfile::tempdir().expect("workspace");
        let providers_dir = tempfile::tempdir().expect("providers dir");
        let provider = bundled_provider("omnifs_provider_github.wasm");

        std::fs::create_dir_all(workspace.path().join("providers/github")).unwrap();
        std::fs::write(
            workspace.path().join("providers/github/Cargo.toml"),
            "[package]\nname = \"omnifs-provider-github\"\n",
        )
        .unwrap();
        let artifact_dir = workspace.path().join("target/wasm32-wasip2/release");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::write(artifact_dir.join("omnifs_provider_github.wasm"), provider).unwrap();
        std::fs::write(artifact_dir.join("stale.wasm"), EMPTY_WASM).unwrap();

        install_target_providers(workspace.path(), providers_dir.path())
            .expect("install providers");

        let index = ProviderStore::new(providers_dir.path())
            .read_index()
            .expect("read index");
        assert_eq!(index.providers.len(), 1);
        assert_eq!(index.providers[0].file, "omnifs_provider_github.wasm");
        assert!(
            !providers_dir
                .path()
                .join("omnifs_provider_github.wasm")
                .exists()
        );
    }

    fn bundled_provider(name: &str) -> Vec<u8> {
        let decoder = zstd::stream::read::Decoder::new(Cursor::new(EMBEDDED_PROVIDER_BUNDLE))
            .expect("decode embedded provider bundle");
        let mut archive = tar::Archive::new(decoder);
        for entry in archive.entries().expect("read embedded provider bundle") {
            let mut entry = entry.expect("read embedded provider bundle entry");
            if entry.path().expect("read embedded provider bundle path") == Path::new(name) {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).expect("read provider bytes");
                return bytes;
            }
        }
        panic!("provider `{name}` missing from embedded bundle");
    }
}
