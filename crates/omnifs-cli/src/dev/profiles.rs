//! Dev profile loading from `contrib/dev-profiles/*.toml`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
struct ProfileFile {
    mounts: Vec<String>,
}

/// Load a named profile from `contrib/dev-profiles/{name}.toml`.
pub fn load(workspace: &Path, name: &str) -> Result<Vec<String>> {
    let path = profile_path(workspace, name);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read dev profile {}", path.display()))?;
    let profile: ProfileFile =
        toml::from_str(&raw).with_context(|| format!("parse dev profile {}", path.display()))?;
    if profile.mounts.is_empty() {
        bail!("dev profile `{name}` must list at least one mount");
    }
    Ok(profile.mounts)
}

pub fn profile_path(workspace: &Path, name: &str) -> PathBuf {
    workspace.join(format!("contrib/dev-profiles/{name}.toml"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn load_reads_profile_mounts() {
        let workspace = tempfile::tempdir().unwrap();
        let profiles_dir = workspace.path().join("contrib/dev-profiles");
        fs::create_dir_all(&profiles_dir).unwrap();
        fs::write(
            profiles_dir.join("test.toml"),
            "mounts = [\"github\", \"db\"]\n",
        )
        .unwrap();

        let mounts = load(workspace.path(), "test").unwrap();
        assert_eq!(mounts, vec!["github".to_string(), "db".to_string()]);
    }

    #[test]
    fn load_rejects_empty_profile() {
        let workspace = tempfile::tempdir().unwrap();
        let profiles_dir = workspace.path().join("contrib/dev-profiles");
        fs::create_dir_all(&profiles_dir).unwrap();
        fs::write(profiles_dir.join("empty.toml"), "mounts = []\n").unwrap();

        let error = load(workspace.path(), "empty").unwrap_err();
        assert!(error.to_string().contains("must list at least one mount"));
    }
}
