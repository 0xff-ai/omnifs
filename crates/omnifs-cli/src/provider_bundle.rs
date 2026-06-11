//! Provider WASM installation into the host runtime home.

use anyhow::{Context as _, anyhow};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::catalog::ProviderCatalog;

const RELEASE_BASE: &str = "https://github.com/0xff-ai/omnifs/releases/download";
const ARCHIVE_TOOL_WASM: &str = "omnifs_tool_archive.wasm";

pub(crate) async fn ensure_release_bundle(providers_dir: &Path) -> anyhow::Result<()> {
    let missing = missing_expected_files(providers_dir)?;
    if missing.is_empty() {
        return Ok(());
    }

    std::fs::create_dir_all(providers_dir)
        .with_context(|| format!("create {}", providers_dir.display()))?;

    let version = env!("CARGO_PKG_VERSION");
    let tag = format!("v{version}");
    anstream::println!(
        "Installing provider WASM bundle into {}",
        providers_dir.display()
    );

    for file in missing {
        let url = format!("{RELEASE_BASE}/{tag}/{file}");
        let target = providers_dir.join(&file);
        download_asset(&url, &target)
            .await
            .with_context(|| format!("install provider asset `{file}` from {url}"))?;
    }
    Ok(())
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

fn missing_expected_files(providers_dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut missing = Vec::new();
    for file in expected_files()? {
        let path = providers_dir.join(&file);
        if !path.is_file() || path.metadata().map(|m| m.len() == 0).unwrap_or(true) {
            missing.push(file);
        }
    }
    Ok(missing)
}

fn expected_files() -> anyhow::Result<Vec<String>> {
    let mut files = BTreeSet::new();
    for manifest in ProviderCatalog::builtin_manifests()? {
        files.insert(manifest.provider);
    }
    files.insert(ARCHIVE_TOOL_WASM.to_string());
    Ok(files.into_iter().collect())
}

async fn download_asset(url: &str, target: &Path) -> anyhow::Result<()> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("fetch {url}"))?;
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("read {url}"))?;
    if bytes.is_empty() {
        return Err(anyhow!("downloaded empty asset"));
    }

    let temp = temp_path(target);
    std::fs::write(&temp, &bytes).with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, target).with_context(|| {
        format!(
            "move downloaded provider asset {} to {}",
            temp.display(),
            target.display()
        )
    })?;
    Ok(())
}

fn temp_path(target: &Path) -> PathBuf {
    let mut temp = target.to_path_buf();
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("provider.wasm");
    temp.set_file_name(format!("{file_name}.tmp-{}", std::process::id()));
    temp
}
