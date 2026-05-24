use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct RepoRoot {
    root: PathBuf,
}

impl RepoRoot {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cargo_toml_path(&self) -> PathBuf {
        self.root.join("Cargo.toml")
    }

    pub fn changelog_path(&self) -> PathBuf {
        self.root.join("CHANGELOG.md")
    }

    pub fn read_workspace_version(&self) -> Result<String> {
        read_workspace_version(self.root())
    }

    pub fn set_workspace_version(&self, version: &str) -> Result<()> {
        set_workspace_version(self.root(), version)
    }

    pub fn refresh_lockfile(&self) -> Result<()> {
        refresh_lockfile(self.root())
    }
}

pub fn find_repo_root(start: impl AsRef<Path>) -> Result<RepoRoot> {
    let mut dir = start.as_ref().canonicalize().context("canonicalize cwd")?;
    loop {
        if dir.join("Cargo.toml").is_file() && dir.join("CHANGELOG.md").is_file() {
            return Ok(RepoRoot { root: dir });
        }
        if !dir.pop() {
            bail!("could not find repo root containing Cargo.toml and CHANGELOG.md");
        }
    }
}

pub fn read_workspace_version(root: &Path) -> Result<String> {
    let cargo = std::fs::read_to_string(root.join("Cargo.toml"))?;
    let doc = cargo.parse::<toml_edit::DocumentMut>()?;
    doc.get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .context("Cargo.toml missing [workspace.package].version")
}

pub fn set_workspace_version(root: &Path, version: &str) -> Result<()> {
    let path = root.join("Cargo.toml");
    let contents = std::fs::read_to_string(&path)?;
    let mut doc = contents.parse::<toml_edit::DocumentMut>()?;
    doc["workspace"]["package"]["version"] = toml_edit::value(version);
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

pub fn refresh_lockfile(root: &Path) -> Result<()> {
    let output = Command::new("cargo")
        .current_dir(root)
        .args(["update", "--workspace"])
        .output()
        .context("spawn cargo update --workspace")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("cargo update --workspace failed: {stderr}");
    }
    Ok(())
}
