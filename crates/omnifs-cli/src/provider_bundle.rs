//! Provider WASM installation into the host runtime home.

use anyhow::Context as _;
use std::collections::BTreeSet;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::catalog::ProviderCatalog;

const ARCHIVE_TOOL_WASM: &str = "omnifs_tool_archive.wasm";

/// Marks `providers_dir` with the id of the bundle last unpacked into it, so a
/// warm launch can skip decompression instead of re-reading every file.
const PROVIDER_BUNDLE_SENTINEL: &str = ".omnifs-provider-bundle";

static EMBEDDED_PROVIDER_BUNDLE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/provider-bundle.tar.zst"));

pub(crate) fn install_embedded_bundle(providers_dir: &Path) -> anyhow::Result<()> {
    let expected = expected_files()?;
    let bundle_id = embedded_bundle_id();

    // Warm path: the exact embedded bundle is already unpacked here, so skip
    // decompression. The sentinel only goes stale when the binary's bundle
    // changes or a provider file is removed.
    if bundle_already_installed(providers_dir, &expected, &bundle_id) {
        return Ok(());
    }

    std::fs::create_dir_all(providers_dir)
        .with_context(|| format!("create {}", providers_dir.display()))?;
    anstream::println!(
        "Installing provider WASM bundle into {}",
        providers_dir.display()
    );

    // The bundle is a compile-time artifact this crate's build script produced
    // from the provider catalog, so its entries are trusted bare file names.
    // The one guard worth keeping catches drift between the packed set and the
    // catalog the host will look up.
    let decoder = zstd::stream::read::Decoder::new(Cursor::new(EMBEDDED_PROVIDER_BUNDLE))
        .context("decode embedded provider bundle")?;
    let mut archive = tar::Archive::new(decoder);
    let mut installed = BTreeSet::new();

    for entry in archive.entries().context("read embedded provider bundle")? {
        let mut entry = entry.context("read embedded provider bundle entry")?;
        let name = {
            let path = entry.path().context("read embedded provider bundle path")?;
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        };
        let Some(name) = name else {
            anyhow::bail!("embedded provider bundle entry has no file name");
        };
        anyhow::ensure!(
            expected.contains(&name),
            "embedded provider bundle contains unexpected file `{name}`"
        );

        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .with_context(|| format!("read embedded provider bundle file `{name}`"))?;
        write_if_changed(providers_dir, &name, &bytes)?;
        installed.insert(name);
    }

    let missing = expected.difference(&installed).cloned().collect::<Vec<_>>();
    anyhow::ensure!(
        missing.is_empty(),
        "embedded provider bundle is missing expected file(s): {}",
        missing.join(", ")
    );

    write_sentinel(providers_dir, &bundle_id)?;
    Ok(())
}

/// Content id of the embedded bundle, stable for a given binary build. Hashing
/// the compressed bytes is far cheaper than the decompression and full re-read
/// it lets a warm launch skip.
fn embedded_bundle_id() -> String {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    EMBEDDED_PROVIDER_BUNDLE.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn bundle_already_installed(
    providers_dir: &Path,
    expected: &BTreeSet<String>,
    bundle_id: &str,
) -> bool {
    let sentinel = providers_dir.join(PROVIDER_BUNDLE_SENTINEL);
    let Ok(stamped) = std::fs::read_to_string(&sentinel) else {
        return false;
    };
    stamped.trim() == bundle_id
        && expected
            .iter()
            .all(|file| providers_dir.join(file).is_file())
}

fn write_sentinel(providers_dir: &Path, bundle_id: &str) -> anyhow::Result<()> {
    let sentinel = providers_dir.join(PROVIDER_BUNDLE_SENTINEL);
    std::fs::write(&sentinel, bundle_id).with_context(|| format!("write {}", sentinel.display()))
}

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
    Ok(())
}

fn expected_files() -> anyhow::Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    for manifest in ProviderCatalog::builtin_manifests()? {
        files.insert(manifest.provider);
    }
    files.insert(ARCHIVE_TOOL_WASM.to_string());
    Ok(files)
}

fn write_if_changed(providers_dir: &Path, file: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let target = providers_dir.join(file);
    if target.is_file()
        && std::fs::read(&target)
            .map(|existing| existing == bytes)
            .unwrap_or(false)
    {
        return Ok(());
    }

    let temp = temp_path(&target);
    std::fs::write(&temp, bytes).with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, &target).with_context(|| {
        format!(
            "move provider bundle file {} to {}",
            temp.display(),
            target.display()
        )
    })?;
    Ok(())
}

fn temp_path(target: impl AsRef<Path>) -> PathBuf {
    let target = target.as_ref();
    let mut temp = target.to_path_buf();
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("provider.wasm");
    temp.set_file_name(format!("{file_name}.tmp-{}", std::process::id()));
    temp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_bundle_installs_expected_files() {
        let providers_dir = tempfile::tempdir().expect("temp providers dir");

        install_embedded_bundle(providers_dir.path()).expect("install embedded provider bundle");
        // Second call hits the sentinel warm path and must stay a no-op success.
        install_embedded_bundle(providers_dir.path()).expect("reinstall embedded provider bundle");

        assert!(
            providers_dir
                .path()
                .join(PROVIDER_BUNDLE_SENTINEL)
                .is_file(),
            "sentinel must record the installed bundle"
        );
        for file in expected_files().expect("expected files") {
            let path = providers_dir.path().join(&file);
            assert!(path.is_file(), "missing {file}");
            assert!(
                path.metadata().expect("provider metadata").len() > 0,
                "{file} is empty"
            );
        }
        assert!(
            !providers_dir.path().join("test_provider.wasm").exists(),
            "test provider must not ship in the embedded bundle"
        );
    }

    #[test]
    fn warm_path_reinstalls_when_a_provider_file_is_removed() {
        let providers_dir = tempfile::tempdir().expect("temp providers dir");
        install_embedded_bundle(providers_dir.path()).expect("install embedded provider bundle");

        let victim = providers_dir.path().join(ARCHIVE_TOOL_WASM);
        std::fs::remove_file(&victim).expect("remove a provider file");

        install_embedded_bundle(providers_dir.path()).expect("reinstall embedded provider bundle");
        assert!(victim.is_file(), "removed provider file must be restored");
    }
}
