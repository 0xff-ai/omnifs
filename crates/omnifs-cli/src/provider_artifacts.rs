//! Native runtime provider artifact provisioning.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use omnifs_host::tools::archive::ARCHIVE_TOOL_WASM;

use crate::dev_support::WorkspaceRoot;
use crate::paths::Paths;
use crate::session::MountConfig;

const WASM_DIR_ENV: &str = "OMNIFS_WASM_DIR";

pub(crate) struct NativeArtifacts<'a> {
    providers_dir: &'a Path,
    sources: Vec<PathBuf>,
}

impl<'a> NativeArtifacts<'a> {
    pub(crate) fn discover(paths: &'a Paths) -> Self {
        Self {
            providers_dir: &paths.providers_dir,
            sources: artifact_sources(),
        }
    }

    pub(crate) fn ensure_for_launch(&self, configs: &[MountConfig]) -> anyhow::Result<()> {
        let required = RequiredArtifacts::from_mounts(configs);
        fs::create_dir_all(self.providers_dir)
            .with_context(|| format!("create {}", self.providers_dir.display()))?;

        let mut copied = 0;
        for artifact in required.files {
            let destination = self.providers_dir.join(&artifact);
            if destination.exists() {
                continue;
            }
            let source = self.source_for(&artifact)?;
            fs::copy(&source, &destination).with_context(|| {
                format!("copy {} to {}", source.display(), destination.display())
            })?;
            copied += 1;
        }

        if copied > 0 {
            anstream::println!(
                "✓ Installed {copied} native provider artifact(s) into {}",
                self.providers_dir.display()
            );
        }
        Ok(())
    }

    fn source_for(&self, artifact: &str) -> anyhow::Result<PathBuf> {
        for source_dir in &self.sources {
            let candidate = source_dir.join(artifact);
            if candidate.exists() {
                return Ok(candidate);
            }
        }

        Err(anyhow!(
            "native runtime needs `{}` in {}, but no source artifact was found",
            artifact,
            self.providers_dir.display()
        ))
        .context(format!(
            "searched: {}",
            self.sources
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .context(format!(
            "build provider artifacts with `just providers-build`, or set {WASM_DIR_ENV} to a directory containing omnifs_provider_*.wasm and {ARCHIVE_TOOL_WASM}"
        ))
    }
}

struct RequiredArtifacts {
    files: BTreeSet<String>,
}

impl RequiredArtifacts {
    fn from_mounts(configs: &[MountConfig]) -> Self {
        let mut files = BTreeSet::from([ARCHIVE_TOOL_WASM.to_string()]);
        for config in configs {
            if let Some(provider) = relative_wasm_file(&config.config.provider) {
                files.insert(provider.to_owned());
            }
        }
        Self { files }
    }
}

fn relative_wasm_file(provider: &str) -> Option<&str> {
    let path = Path::new(provider);
    if path.is_absolute() {
        return None;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("wasm"))
        })
}

fn artifact_sources() -> Vec<PathBuf> {
    let mut sources = Vec::new();
    if let Some(path) = std::env::var_os(WASM_DIR_ENV).map(PathBuf::from) {
        sources.push(path);
    }
    if let Ok(workspace) = WorkspaceRoot::discover() {
        sources.push(workspace.path().join("target/wasm32-wasip2/release"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        sources.push(dir.join("providers"));
        sources.push(dir.to_path_buf());
    }
    dedupe_existing_order(sources)
}

fn dedupe_existing_order(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter_map(|path| {
            let key = comparable_path(&path).unwrap_or_else(|| path.clone());
            seen.insert(key).then_some(path)
        })
        .collect()
}

fn comparable_path(path: &Path) -> Option<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Some(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(provider: &str) -> MountConfig {
        let raw = format!(r#"{{"provider":"{provider}","mount":"test"}}"#);
        MountConfig::from_parsed(
            omnifs_host::mounts::Spec::parse(&raw).unwrap(),
            PathBuf::from("test.json"),
        )
        .unwrap()
    }

    #[test]
    fn required_artifacts_include_archive_tool_and_relative_mount_providers() {
        let required = RequiredArtifacts::from_mounts(&[
            mount("omnifs_provider_github.wasm"),
            mount("/tmp/custom_provider.wasm"),
        ]);

        assert!(required.files.contains(ARCHIVE_TOOL_WASM));
        assert!(required.files.contains("omnifs_provider_github.wasm"));
        assert!(!required.files.contains("custom_provider.wasm"));
    }

    #[test]
    fn ensure_for_launch_copies_missing_artifacts_from_source_bundle() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        fs::write(source.path().join(ARCHIVE_TOOL_WASM), b"archive").unwrap();
        fs::write(source.path().join("omnifs_provider_github.wasm"), b"github").unwrap();

        NativeArtifacts {
            providers_dir: destination.path(),
            sources: vec![source.path().to_path_buf()],
        }
        .ensure_for_launch(&[mount("omnifs_provider_github.wasm")])
        .unwrap();

        assert_eq!(
            fs::read(destination.path().join(ARCHIVE_TOOL_WASM)).unwrap(),
            b"archive"
        );
        assert_eq!(
            fs::read(destination.path().join("omnifs_provider_github.wasm")).unwrap(),
            b"github"
        );
    }
}
