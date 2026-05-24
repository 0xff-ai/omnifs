use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

pub fn create_release(
    root: &Path,
    tag: &str,
    title: &str,
    notes: &str,
    target_sha: &str,
) -> Result<()> {
    let notes_file = std::env::temp_dir().join(format!("omnifs-release-{tag}.md"));
    std::fs::write(&notes_file, notes)?;

    let output = Command::new("gh")
        .current_dir(root)
        .args([
            "release",
            "create",
            tag,
            "--target",
            target_sha,
            "--title",
            title,
            "--notes-file",
            notes_file.to_str().context("non-utf8 temp path")?,
        ])
        .output()
        .context("spawn gh release create")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh release create failed: {stderr}");
    }
    Ok(())
}

pub fn create_pull_request(root: &Path, branch: &str, version: &str) -> Result<()> {
    let title = format!("release: v{version}");
    let body = format!(
        "Prepare omnifs v{version}.\n\n\
         - Finalizes CHANGELOG.md\n\
         - Bumps workspace, lockfile, and npm versions\n\n\
         Merging this PR triggers tag creation and the ship pipeline."
    );
    let output = Command::new("gh")
        .current_dir(root)
        .args([
            "pr", "create", "--base", "main", "--head", branch, "--title", &title, "--body", &body,
            "--label", "release",
        ])
        .output()
        .context("spawn gh pr create")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh pr create failed: {stderr}");
    }
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}
