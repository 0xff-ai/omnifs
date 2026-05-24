use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn ensure_clean_tree(root: &Path) -> Result<()> {
    let status = git(root, ["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        bail!("working tree is not clean; commit or stash changes before preparing a release");
    }
    Ok(())
}

pub fn ensure_on_branch(root: &Path, branch: &str) -> Result<()> {
    let current = git(root, ["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    if current != branch {
        bail!("expected to be on branch {branch}, but on {current}");
    }
    Ok(())
}

pub fn create_branch(root: &Path, branch: &str) -> Result<()> {
    git(root, ["checkout", "-b", branch])?;
    Ok(())
}

pub fn commit_all(root: &Path, message: &str) -> Result<()> {
    git(root, ["add", "-A"])?;
    git(root, ["commit", "-m", message])?;
    Ok(())
}

pub fn push_branch(root: &Path, branch: &str) -> Result<()> {
    git(root, ["push", "-u", "origin", branch])?;
    Ok(())
}

pub fn diff_name_only(root: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    let output = git(root, ["diff", "--name-only", &format!("{base}...{head}")])?;
    Ok(output
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

pub fn show_file_at(root: &Path, rev: &str, path: &str) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["show", &format!("{rev}:{path}")])
        .output()
        .context("spawn git show")?;
    if !output.status.success() {
        bail!("git show {rev}:{path} failed");
    }
    Ok(output.stdout)
}

pub fn latest_semver_tag(root: &Path) -> Result<Option<String>> {
    let output = git(root, ["tag", "-l", "v*.*.*", "--sort=-v:refname"])?;
    Ok(output.lines().next().map(str::to_owned))
}

pub fn head_commit(root: &Path) -> Result<String> {
    Ok(git(root, ["rev-parse", "HEAD"])?.trim().to_string())
}

pub fn resolve_tag_commit(root: &Path, tag: &str) -> Result<String> {
    Ok(
        git(root, ["rev-list", "-n", "1", &format!("{tag}^{{commit}}")])?
            .trim()
            .to_string(),
    )
}

fn git(root: &Path, args: impl IntoIterator<Item = impl AsRef<str>>) -> Result<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args.into_iter().map(|arg| arg.as_ref().to_string()))
        .output()
        .context("spawn git")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git command failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
