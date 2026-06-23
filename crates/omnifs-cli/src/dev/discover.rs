//! Dev mount discovery from `providers/*/dev/mount.json`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result};

/// A dev mount discovered from `providers/<dir>/dev/mount.json`, before its
/// provider name is pinned to a content-addressed reference.
///
/// The on-disk file authors `provider` as a bare name (e.g. `github`); pinning
/// to a `ProviderRef` happens once the workspace providers are installed and the
/// catalog can resolve the name. The raw JSON is carried verbatim so every other
/// field (auth, capabilities, config) resolves through `Spec`'s own
/// deserialization at pin time.
#[derive(Debug, Clone)]
pub struct DiscoveredMount {
    pub provider_name: String,
    pub mount_name: String,
    pub raw: serde_json::Value,
}

/// Read `providers/*/dev/mount.json` from the workspace root, keyed by mount name.
pub fn discover(workspace: &Path) -> Result<BTreeMap<String, DiscoveredMount>> {
    let providers = workspace.join("providers");
    let mut mounts = BTreeMap::new();
    let entries = match std::fs::read_dir(&providers) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(mounts),
        Err(error) => return Err(error).with_context(|| format!("scan {}", providers.display())),
    };
    for entry in entries {
        let path = entry
            .with_context(|| format!("scan {}", providers.display()))?
            .path()
            .join("dev/mount.json");
        if !path.is_file() {
            continue;
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("read dev mount {}", path.display()))?;
        let raw: serde_json::Value = serde_json::from_str(&contents)
            .with_context(|| format!("parse dev mount {}", path.display()))?;
        let provider_name = raw
            .get("provider")
            .and_then(|value| value.as_str())
            .with_context(|| format!("dev mount {} must set a string `provider`", path.display()))?
            .to_owned();
        let mount_name = raw
            .get("mount")
            .and_then(|value| value.as_str())
            .with_context(|| format!("dev mount {} must set a string `mount`", path.display()))?
            .to_owned();
        mounts.insert(
            mount_name.clone(),
            DiscoveredMount {
                provider_name,
                mount_name,
                raw,
            },
        );
    }
    Ok(mounts)
}

pub fn filter_by_profile(
    discovered: &BTreeMap<String, DiscoveredMount>,
    profile_mounts: &[String],
) -> Result<Vec<DiscoveredMount>> {
    let mut selected = Vec::new();
    for name in profile_mounts {
        match discovered.get(name) {
            Some(entry) => selected.push(entry.clone()),
            None => anyhow::bail!(
                "profile references mount `{name}` but no providers/*/dev/mount.json defines it"
            ),
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write_mount_json(dir: &Path, provider: &str, mount: &str) {
        let dev_dir = dir.join(format!("providers/{provider}/dev"));
        fs::create_dir_all(&dev_dir).unwrap();
        fs::write(
            dev_dir.join("mount.json"),
            format!(
                r#"{{
  "provider": "{provider}",
  "mount": "{mount}",
  "auth": {{ "type": "static-token", "scheme": "pat" }}
}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn discover_finds_provider_dev_mounts() {
        let workspace = tempfile::tempdir().unwrap();
        write_mount_json(workspace.path(), "github", "github");
        write_mount_json(workspace.path(), "db", "db");

        let mounts = discover(workspace.path()).unwrap();
        assert_eq!(mounts.len(), 2);
        assert!(mounts.contains_key("github"));
        assert!(mounts.contains_key("db"));
        assert_eq!(mounts["github"].provider_name, "github");
    }

    #[test]
    fn filter_by_profile_selects_requested_mounts() {
        let workspace = tempfile::tempdir().unwrap();
        write_mount_json(workspace.path(), "github", "github");
        write_mount_json(workspace.path(), "db", "db");

        let discovered = discover(workspace.path()).unwrap();
        let selected = filter_by_profile(&discovered, &["github".to_string()]).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].mount_name, "github");
    }

    #[test]
    fn filter_by_profile_errors_on_missing_mount() {
        let workspace = tempfile::tempdir().unwrap();
        write_mount_json(workspace.path(), "github", "github");
        let discovered = discover(workspace.path()).unwrap();
        let error = filter_by_profile(&discovered, &["missing".to_string()]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("profile references mount `missing`")
        );
    }
}
