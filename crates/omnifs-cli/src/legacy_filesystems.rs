//! Read-only discovery of pre-resource client filesystem data.
//!
//! This module never creates, changes, launches, or treats legacy specs as
//! desired Attachments. It exists solely for migration hints and stopped-daemon
//! Doctor probes.

use anyhow::Context as _;
use omnifs_core::{AttachmentProtocol, AttachmentRuntime, ResourceName};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILESYSTEMS_DIR: &str = "filesystems";
const SPECS_DIR: &str = "specs";

#[derive(Debug, Clone)]
pub(crate) struct LegacyFilesystems {
    profile_root: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct LegacyScan {
    pub(crate) specs: Vec<LegacyFilesystemSpec>,
    pub(crate) issues: Vec<LegacyIssue>,
}

/// Strict DTO for one pre-resource filesystem spec. This type only supports
/// read-only migration and Doctor reporting. It never becomes desired state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyFilesystemSpec {
    id: String,
    protocol: AttachmentProtocol,
    runtime: AttachmentRuntime,
    location: PathBuf,
}

impl LegacyFilesystemSpec {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) const fn runtime(&self) -> AttachmentRuntime {
        self.runtime
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LegacyIssue {
    pub(crate) path: PathBuf,
    pub(crate) message: String,
}

impl LegacyFilesystems {
    #[must_use]
    pub(crate) fn under_profile(profile_root: impl Into<PathBuf>) -> Self {
        Self {
            profile_root: profile_root.into(),
        }
    }

    #[must_use]
    pub(crate) fn profile_root(&self) -> &Path {
        &self.profile_root
    }

    fn root(&self) -> PathBuf {
        self.profile_root.join("client").join(FILESYSTEMS_DIR)
    }

    fn specs_root(&self) -> PathBuf {
        self.root().join(SPECS_DIR)
    }

    pub(crate) fn runtime_paths(&self) -> anyhow::Result<omnifs_fs_runtime::RuntimePaths> {
        let root = self.root();
        Ok(omnifs_fs_runtime::RuntimePaths::new(
            self.profile_root.clone(),
            std::env::var_os(omnifs_bootstrap::OMNIFS_HOME_ENV).is_none(),
            root.join("state"),
            self.profile_root.join("client").join("cache"),
            root.join("runtime"),
            self.profile_root.join("client/cache/guest-images"),
            std::env::current_exe().context("resolve the omnifs executable")?,
        ))
    }

    pub(crate) fn scan(&self) -> anyhow::Result<LegacyScan> {
        let root = self.specs_root();
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LegacyScan {
                    specs: Vec::new(),
                    issues: Vec::new(),
                });
            },
            Err(error) => {
                return Err(error).with_context(|| format!("scan legacy specs {}", root.display()));
            },
        };
        let mut paths = Vec::new();
        let mut issues = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry)
                    if entry.path().extension().and_then(|value| value.to_str())
                        == Some("json") =>
                {
                    paths.push(entry.path());
                },
                Ok(_) => {},
                Err(error) => issues.push(LegacyIssue {
                    path: root.clone(),
                    message: format!("read legacy spec directory entry: {error}"),
                }),
            }
        }
        paths.sort();
        let mut specs = Vec::new();
        for path in paths {
            match Self::read(&path) {
                Ok(spec) => specs.push(spec),
                Err(error) => issues.push(LegacyIssue {
                    path,
                    message: format!("{error:#}"),
                }),
            }
        }
        Ok(LegacyScan { specs, issues })
    }

    fn read(path: &Path) -> anyhow::Result<LegacyFilesystemSpec> {
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid legacy spec filename {}", path.display()))?;
        ResourceName::new(stem)
            .with_context(|| format!("invalid legacy spec filename {}", path.display()))?;
        let bytes =
            std::fs::read(path).with_context(|| format!("read legacy spec {}", path.display()))?;
        let spec: LegacyFilesystemSpec = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse legacy spec {}", path.display()))?;
        ResourceName::new(spec.id())
            .with_context(|| format!("invalid legacy spec {}", path.display()))?;
        anyhow::ensure!(
            spec.id() == stem,
            "legacy spec {} declares `{}`",
            path.display(),
            spec.id()
        );
        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    fn spec(id: &str, location: &Path) -> LegacyFilesystemSpec {
        LegacyFilesystemSpec {
            id: id.to_owned(),
            protocol: AttachmentProtocol::Nfs,
            runtime: AttachmentRuntime::Host,
            location: location.to_path_buf(),
        }
    }

    fn write_spec(profile: &Path, file: &str, value: &serde_json::Value) -> PathBuf {
        let root = profile.join("client/filesystems/specs");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(file);
        std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path
    }

    #[test]
    fn absent_legacy_tree_scans_empty_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let client = dir.path().join("client");
        let scan = LegacyFilesystems::under_profile(dir.path()).scan().unwrap();
        assert!(scan.specs.is_empty());
        assert!(scan.issues.is_empty());
        assert!(!client.exists());
    }

    #[test]
    fn valid_specs_are_sorted_and_scanning_changes_no_file_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let beta = write_spec(
            dir.path(),
            "beta.json",
            &serde_json::to_value(spec("beta", &dir.path().join("beta"))).unwrap(),
        );
        write_spec(
            dir.path(),
            "alpha.json",
            &serde_json::to_value(spec("alpha", &dir.path().join("alpha"))).unwrap(),
        );
        std::fs::set_permissions(&beta, std::fs::Permissions::from_mode(0o640)).unwrap();
        let before = std::fs::metadata(&beta).unwrap();
        let bytes = std::fs::read(&beta).unwrap();

        let scan = LegacyFilesystems::under_profile(dir.path()).scan().unwrap();
        assert_eq!(
            scan.specs
                .iter()
                .map(LegacyFilesystemSpec::id)
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert!(scan.issues.is_empty());
        let after = std::fs::metadata(&beta).unwrap();
        assert_eq!(std::fs::read(&beta).unwrap(), bytes);
        assert_eq!(after.mode(), before.mode());
        assert_eq!(after.mtime(), before.mtime());
        assert_eq!(after.mtime_nsec(), before.mtime_nsec());
    }

    #[test]
    fn corrupt_and_mismatched_specs_do_not_hide_valid_entries() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "valid.json",
            &serde_json::to_value(spec("valid", &dir.path().join("valid"))).unwrap(),
        );
        let mut unknown =
            serde_json::to_value(spec("unknown", &dir.path().join("unknown"))).unwrap();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_owned(), serde_json::json!(true));
        write_spec(dir.path(), "unknown.json", &unknown);
        write_spec(
            dir.path(),
            "filename.json",
            &serde_json::to_value(spec("different", &dir.path().join("different"))).unwrap(),
        );

        let scan = LegacyFilesystems::under_profile(dir.path()).scan().unwrap();
        assert_eq!(scan.specs.len(), 1);
        assert_eq!(scan.specs[0].id(), "valid");
        assert_eq!(scan.issues.len(), 2);
        assert!(
            scan.issues
                .iter()
                .any(|issue| issue.message.contains("unknown field"))
        );
        assert!(
            scan.issues
                .iter()
                .any(|issue| issue.message.contains("declares `different`"))
        );
    }
}
