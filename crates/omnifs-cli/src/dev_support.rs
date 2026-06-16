//! Contributor dev workflow helpers.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, anyhow};

use crate::error::WithHint;

pub(crate) struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    pub(crate) fn discover() -> anyhow::Result<Self> {
        let cwd = std::env::current_dir().context("read cwd")?;
        for dir in cwd.ancestors() {
            let manifest = dir.join("Cargo.toml");
            match fs::read_to_string(&manifest) {
                Ok(content) if content.contains("[workspace]") => {
                    return Ok(Self(dir.to_path_buf()));
                },
                Ok(_) => {},
                Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                Err(error) => {
                    return Err(error).with_context(|| format!("read {}", manifest.display()));
                },
            }
        }
        Err(anyhow!(
            "`omnifs dev` must run inside the omnifs source checkout; no [workspace] Cargo.toml found above {}",
            cwd.display()
        ))
        .with_hint("Clone https://github.com/0xff-ai/omnifs and run `omnifs dev` from the repo root")
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

pub(crate) struct DevImageTag(String);

impl DevImageTag {
    pub(crate) fn synthesize(workspace: &WorkspaceRoot) -> anyhow::Result<Self> {
        let output = Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .current_dir(workspace.path())
            .output()
            .context("invoke git rev-parse")?;
        if !output.status.success() {
            anyhow::bail!(
                "git rev-parse failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let commit = String::from_utf8(output.stdout)
            .context("git rev-parse output was not UTF-8")?
            .trim()
            .to_string();
        if commit.is_empty() {
            anyhow::bail!("git rev-parse returned an empty commit hash");
        }
        Ok(Self(format!("omnifs:{commit}-dev")))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
