//! Git-backed desired state for mount specifications.
//!
//! [`Repository`] owns the Git process boundary and holds the mount registry
//! lock for its entire lifetime. The registry remains the only parser,
//! filename validator, and atomic writer for individual specs.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::{Name, Registry, SpecError};

/// An immutable Git commit id used for persisted mount desired state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Revision(String);

impl Revision {
    pub fn new(value: impl Into<String>) -> Result<Self, RevisionError> {
        let value = value.into();
        let valid_length = matches!(value.len(), 40 | 64);
        if !valid_length
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(RevisionError);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Revision {
    type Err = RevisionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl AsRef<str> for Revision {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// A revision failed validation. Git object ids are lowercase 40- or 64-digit
/// hexadecimal strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid Git revision (expected 40 or 64 lowercase hexadecimal characters)")]
pub struct RevisionError;

/// Errors at the mount desired-state repository boundary.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error(transparent)]
    Spec(#[from] SpecError),
    #[error("failed to access mount repository at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("invalid mount repository revision `{revision}`: {source}")]
    Revision {
        revision: String,
        #[source]
        source: RevisionError,
    },
    #[error("Git {purpose} at {path} failed: {stderr}")]
    Git {
        purpose: String,
        path: PathBuf,
        stderr: String,
    },
    #[error("mount repository contains malformed or invalid specs: {details}")]
    InvalidSpecs { details: String },
    #[error("mount repository contains unexpected tracked file `{path}`")]
    UnexpectedTracked { path: String },
    #[error(
        "mount repository at {path} has configured Git remote `{remote}`; remove it before using omnifs desired state"
    )]
    ConfiguredRemote { path: PathBuf, remote: String },
    #[error("snapshot for revision {revision} at {path} is invalid: {details}")]
    InvalidSnapshot {
        revision: Revision,
        path: PathBuf,
        details: String,
    },
}

/// The local Git repository containing mount desired state.
#[derive(Debug)]
pub struct Repository {
    mounts_dir: PathBuf,
    registry: Registry,
}

impl Repository {
    /// Read the current mount specs and Git refs without initializing, locking,
    /// staging, or committing the repository.
    pub fn observe(mounts_dir: impl AsRef<Path>) -> Result<Self, RepositoryError> {
        let mounts_dir = mounts_dir.as_ref().to_path_buf();
        fs::create_dir_all(&mounts_dir).map_err(|source| RepositoryError::Io {
            path: mounts_dir.clone(),
            source,
        })?;
        Ok(Self {
            registry: Registry::load(&mounts_dir)?,
            mounts_dir,
        })
    }

    /// Open (and, on first use, initialize) the repository at `mounts_dir`.
    /// The registry lock is retained until this value is dropped.
    pub fn open(mounts_dir: impl AsRef<Path>) -> Result<Self, RepositoryError> {
        let mounts_dir = mounts_dir.as_ref().to_path_buf();
        fs::create_dir_all(&mounts_dir).map_err(|source| RepositoryError::Io {
            path: mounts_dir.clone(),
            source,
        })?;
        let registry = Registry::load_locked(&mounts_dir)?;
        let mut repository = Self {
            mounts_dir,
            registry,
        };
        repository.ensure_git_repository()?;
        repository.validate_registry()?;
        repository.validate_tracked_files(None)?;
        repository.commit()?;
        Ok(repository)
    }

    #[must_use]
    pub fn mounts_dir(&self) -> &Path {
        &self.mounts_dir
    }

    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    #[must_use]
    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    /// Return the immutable commit currently at `HEAD`.
    pub fn head_revision(&self) -> Result<Option<Revision>, RepositoryError> {
        let output = self.git(&["rev-parse", "--verify", "HEAD"], "read HEAD")?;
        if !output.status.success() {
            return Ok(None);
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Revision::new(value.clone())
            .map(Some)
            .map_err(|source| RepositoryError::Revision {
                revision: value,
                source,
            })
    }

    /// Persist a spec through Registry's validation and atomic-write path.
    pub fn put(&mut self, spec: &super::Spec) -> Result<(), RepositoryError> {
        self.registry.put(spec)?;
        Ok(())
    }

    /// Remove a spec through Registry's canonical naming and file-removal path.
    pub fn remove(&mut self, name: &Name) -> Result<bool, RepositoryError> {
        Ok(self.registry.remove(name)?)
    }

    /// Commit valid current `*.json` edits and return the resulting revision.
    /// A clean repository returns its existing `HEAD` without creating a new
    /// commit.
    pub fn commit(&mut self) -> Result<Revision, RepositoryError> {
        self.registry.reload()?;
        self.validate_registry()?;
        self.validate_tracked_files(None)?;
        self.stage_specs()?;
        let has_head = self.has_head()?;
        let staged = self.git_status_cached()?;
        if !staged && has_head {
            return self.require_head();
        }
        let message = if has_head {
            "Update mount specs"
        } else {
            "Initialize mount repository"
        };
        self.git(&["commit", "--allow-empty", "-m", message], "commit")?;
        self.require_head()
    }

    /// Create an immutable registry snapshot for `revision` under the
    /// workspace cache. Existing snapshots are validated and reused.
    pub fn snapshot(
        &self,
        revision: &Revision,
        cache_dir: impl AsRef<Path>,
    ) -> Result<(PathBuf, Registry), RepositoryError> {
        self.verify_revision(revision)?;
        let snapshot = Self::snapshot_path(cache_dir.as_ref(), revision);
        if !snapshot.exists() {
            let parent = snapshot.parent().unwrap_or(cache_dir.as_ref());
            fs::create_dir_all(parent).map_err(|source| RepositoryError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            let temporary = parent.join(format!(".{}.tmp-{}", revision, std::process::id()));
            let _ = fs::remove_dir_all(&temporary);
            fs::create_dir(&temporary).map_err(|source| RepositoryError::Io {
                path: temporary.clone(),
                source,
            })?;
            for relative in self.revision_files(revision)? {
                let blob = self.git(
                    &["show", &format!("{revision}:{relative}")],
                    "read snapshot spec",
                )?;
                Registry::write_snapshot_file(&temporary, Path::new(&relative), &blob.stdout)?;
            }
            fs::rename(&temporary, &snapshot).map_err(|source| RepositoryError::Io {
                path: snapshot.clone(),
                source,
            })?;
            make_read_only(&snapshot).map_err(|source| RepositoryError::Io {
                path: snapshot.clone(),
                source,
            })?;
        }
        let registry = self.validate_snapshot(revision, &snapshot)?;
        Ok((snapshot, registry))
    }

    pub(crate) fn snapshot_path(cache_dir: &Path, revision: &Revision) -> PathBuf {
        cache_dir
            .join(crate::layout::MOUNT_REVISIONS_SUBDIR)
            .join(revision.as_str())
    }

    /// Return the revision recorded in `refs/omnifs/applied`, if present.
    pub fn applied(&self) -> Result<Option<Revision>, RepositoryError> {
        let output = self.git(
            &["rev-parse", "--verify", "refs/omnifs/applied"],
            "read applied ref",
        )?;
        if !output.status.success() {
            return Ok(None);
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Revision::new(value.clone())
            .map(Some)
            .map_err(|source| RepositoryError::Revision {
                revision: value,
                source,
            })
    }

    /// Explicitly move `refs/omnifs/applied` to `revision`.
    pub fn mark_applied(&self, revision: &Revision) -> Result<(), RepositoryError> {
        self.verify_revision(revision)?;
        self.git(
            &["update-ref", "refs/omnifs/applied", revision.as_str()],
            "mark applied ref",
        )?;
        Ok(())
    }

    fn ensure_git_repository(&self) -> Result<(), RepositoryError> {
        if !self.mounts_dir.exists() {
            fs::create_dir_all(&self.mounts_dir).map_err(|source| RepositoryError::Io {
                path: self.mounts_dir.clone(),
                source,
            })?;
        }
        if !self.mounts_dir.join(".git").exists() {
            self.git(&["init", "--quiet"], "initialize repository")?;
        }
        self.git(
            &["config", "--local", "user.name", "omnifs"],
            "configure author",
        )?;
        self.git(
            &["config", "--local", "user.email", "omnifs@localhost"],
            "configure author",
        )?;
        self.ensure_lock_ignored()?;
        let remotes = self.git(&["remote"], "list remotes")?;
        if let Some(remote) = String::from_utf8_lossy(&remotes.stdout)
            .lines()
            .find(|line| !line.is_empty())
        {
            return Err(RepositoryError::ConfiguredRemote {
                path: self.mounts_dir.clone(),
                remote: remote.to_string(),
            });
        }
        Ok(())
    }

    fn ensure_lock_ignored(&self) -> Result<(), RepositoryError> {
        let exclude = self.mounts_dir.join(".git").join("info").join("exclude");
        let existing = match fs::read(&exclude) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(source) => {
                return Err(RepositoryError::Io {
                    path: exclude,
                    source,
                });
            },
        };
        if existing
            .split(|byte| *byte == b'\n')
            .any(|line| line == b".lock")
        {
            return Ok(());
        }
        let mut updated = existing;
        if !updated.is_empty() && !updated.ends_with(b"\n") {
            updated.push(b'\n');
        }
        updated.extend_from_slice(b".lock\n");
        fs::write(&exclude, updated).map_err(|source| RepositoryError::Io {
            path: exclude,
            source,
        })
    }

    fn validate_registry(&self) -> Result<(), RepositoryError> {
        if self.registry.failures().is_empty() {
            return Ok(());
        }
        let details = self
            .registry
            .failures()
            .iter()
            .map(|failure| format!("{}: {}", failure.path.display(), failure.error))
            .collect::<Vec<_>>()
            .join("; ");
        Err(RepositoryError::InvalidSpecs { details })
    }

    fn validate_tracked_files(&self, revision: Option<&Revision>) -> Result<(), RepositoryError> {
        let output = match revision {
            Some(revision) => self.git(
                &["ls-tree", "-r", "--name-only", "-z", revision.as_str()],
                "list revision files",
            )?,
            None => self.git(&["ls-files", "-z"], "list tracked files")?,
        };
        let expected = self
            .registry
            .iter()
            .map(|(name, _)| format!("{name}.json"))
            .collect::<BTreeSet<_>>();
        for path in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path = String::from_utf8_lossy(path).to_string();
            let on_disk = self.mounts_dir.join(&path).exists();
            if (!Path::new(&path)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
                || path.contains('/')
                || path.contains('\\'))
                || (on_disk && !expected.contains(&path))
            {
                return Err(RepositoryError::UnexpectedTracked { path });
            }
        }
        Ok(())
    }

    fn revision_files(&self, revision: &Revision) -> Result<Vec<String>, RepositoryError> {
        let output = self.git(
            &["ls-tree", "-r", "-z", revision.as_str()],
            "list revision files",
        )?;
        let mut files = Vec::new();
        for entry in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let entry = String::from_utf8_lossy(entry);
            let Some((header, path)) = entry.split_once('\t') else {
                return Err(RepositoryError::UnexpectedTracked {
                    path: entry.to_string(),
                });
            };
            let mut fields = header.split_ascii_whitespace();
            let mode = fields.next().unwrap_or_default();
            let kind = fields.next().unwrap_or_default();
            let _object = fields.next().unwrap_or_default();
            if kind != "blob"
                || !matches!(mode, "100644" | "100755")
                || path.contains('/')
                || !Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            {
                return Err(RepositoryError::UnexpectedTracked {
                    path: path.to_string(),
                });
            }
            files.push(path.to_string());
        }
        Ok(files)
    }

    fn stage_specs(&self) -> Result<(), RepositoryError> {
        let tracked = self.git(&["ls-files", "-z"], "list tracked files")?;
        if tracked.stdout.iter().any(|byte| *byte != 0) {
            self.git(&["add", "-u", "--", "."], "stage tracked mount specs")?;
        }
        let names = self
            .registry
            .iter()
            .map(|(name, _)| format!("{name}.json"))
            .collect::<Vec<_>>();
        if !names.is_empty() {
            let mut args = vec!["add", "--"];
            args.extend(names.iter().map(String::as_str));
            self.git(&args, "stage mount specs")?;
        }
        Ok(())
    }

    fn has_head(&self) -> Result<bool, RepositoryError> {
        Ok(self.head_revision()?.is_some())
    }

    fn require_head(&self) -> Result<Revision, RepositoryError> {
        self.head_revision()?.ok_or_else(|| RepositoryError::Git {
            purpose: "read HEAD".to_string(),
            path: self.mounts_dir.clone(),
            stderr: "repository has no HEAD after commit".to_string(),
        })
    }

    fn git_status_cached(&self) -> Result<bool, RepositoryError> {
        let output = self.git(&["diff", "--cached", "--quiet"], "inspect staged changes")?;
        Ok(!output.status.success())
    }

    fn verify_revision(&self, revision: &Revision) -> Result<(), RepositoryError> {
        let output = self.git(
            &["cat-file", "-e", &format!("{revision}^{{commit}}")],
            "verify revision",
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(RepositoryError::Git {
                purpose: "verify revision".to_string(),
                path: self.mounts_dir.clone(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    fn validate_snapshot(
        &self,
        revision: &Revision,
        snapshot: &Path,
    ) -> Result<Registry, RepositoryError> {
        let root = fs::symlink_metadata(snapshot).map_err(|source| RepositoryError::Io {
            path: snapshot.to_path_buf(),
            source,
        })?;
        if !root.file_type().is_dir() {
            return Err(RepositoryError::InvalidSnapshot {
                revision: revision.clone(),
                path: snapshot.to_path_buf(),
                details: "snapshot root is not a real directory".to_string(),
            });
        }
        let expected = self.revision_files(revision)?;
        let expected = expected.into_iter().collect::<BTreeSet<_>>();
        let mut actual = BTreeSet::new();
        for entry in fs::read_dir(snapshot).map_err(|source| RepositoryError::Io {
            path: snapshot.to_path_buf(),
            source,
        })? {
            let entry = entry.map_err(|source| RepositoryError::Io {
                path: snapshot.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let name =
                entry
                    .file_name()
                    .into_string()
                    .map_err(|_| RepositoryError::InvalidSnapshot {
                        revision: revision.clone(),
                        path: path.clone(),
                        details: "snapshot entry name is not UTF-8".to_string(),
                    })?;
            let metadata = fs::symlink_metadata(&path).map_err(|source| RepositoryError::Io {
                path: path.clone(),
                source,
            })?;
            if !metadata.file_type().is_file() {
                return Err(RepositoryError::InvalidSnapshot {
                    revision: revision.clone(),
                    path,
                    details: "snapshot entry is not a regular non-symlink file".to_string(),
                });
            }
            actual.insert(name);
        }
        if actual != expected {
            return Err(RepositoryError::InvalidSnapshot {
                revision: revision.clone(),
                path: snapshot.to_path_buf(),
                details: "snapshot file set differs from the requested revision".to_string(),
            });
        }
        let registry = Registry::load(snapshot)?;
        if let Some(failure) = registry.failures().first() {
            return Err(RepositoryError::InvalidSnapshot {
                revision: revision.clone(),
                path: snapshot.to_path_buf(),
                details: failure.error.to_string(),
            });
        }
        for relative in expected {
            let expected = self.git(
                &["show", &format!("{revision}:{relative}")],
                "read snapshot spec",
            )?;
            let actual =
                fs::read(snapshot.join(&relative)).map_err(|source| RepositoryError::Io {
                    path: snapshot.join(&relative),
                    source,
                })?;
            if expected.stdout != actual {
                return Err(RepositoryError::InvalidSnapshot {
                    revision: revision.clone(),
                    path: snapshot.join(relative),
                    details: "file bytes differ from the requested revision".to_string(),
                });
            }
        }
        Ok(registry)
    }

    fn git(&self, args: &[&str], purpose: &str) -> Result<Output, RepositoryError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.mounts_dir)
            .args(args)
            .output()
            .map_err(|source| RepositoryError::Io {
                path: self.mounts_dir.clone(),
                source,
            })?;
        if output.status.success()
            || (purpose == "read HEAD" && output.status.code() == Some(128))
            || (purpose == "read applied ref" && output.status.code() == Some(128))
            || purpose == "inspect staged changes"
        {
            return Ok(output);
        }
        Err(RepositoryError::Git {
            purpose: purpose.to_string(),
            path: self.mounts_dir.clone(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

#[cfg(unix)]
fn make_read_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    for entry in walk(path)? {
        let mode = if entry.is_dir() { 0o555 } else { 0o444 };
        fs::set_permissions(entry, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_read_only(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn walk(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = vec![root.to_path_buf()];
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            paths.extend(walk(&path)?);
        } else {
            paths.push(path);
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ProviderId, ProviderMeta, ProviderName, ProviderRef};

    fn spec(name: &str) -> super::super::Spec {
        serde_json::from_value(serde_json::json!({
            "provider": ProviderRef {
                id: ProviderId::from_wasm_bytes(name.as_bytes()),
                meta: ProviderMeta {
                    name: ProviderName::new(name).unwrap(),
                    version: None,
                },
            },
            "mount": name,
        }))
        .unwrap()
    }

    fn git(path: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap()
    }

    #[test]
    fn bootstrap_uses_fixed_author_and_no_remote() {
        let dir = tempfile::tempdir().unwrap();
        let mounts = dir.path().join("mounts");
        fs::create_dir_all(&mounts).unwrap();
        fs::write(
            mounts.join("demo.json"),
            serde_json::to_vec(&spec("demo")).unwrap(),
        )
        .unwrap();

        let repository = Repository::open(&mounts).unwrap();
        let author = git(&mounts, &["show", "-s", "--format=%an <%ae>"]);
        assert_eq!(
            String::from_utf8_lossy(&author.stdout).trim(),
            "omnifs <omnifs@localhost>"
        );
        assert!(git(&mounts, &["remote"]).stdout.is_empty());
        assert!(repository.applied().unwrap().is_none());
        assert!(repository.registry().failures().is_empty());
        let status = git(&mounts, &["status", "--porcelain", "--untracked-files=all"]);
        assert!(status.status.success());
        assert!(
            status.stdout.is_empty(),
            "the retained registry lock must stay ignored: {:?}",
            String::from_utf8_lossy(&status.stdout)
        );
        drop(repository);
        fs::write(
            mounts.join("extra.json"),
            serde_json::to_vec(&spec("extra")).unwrap(),
        )
        .unwrap();
        let reopened = Repository::open(&mounts).unwrap();
        let tracked = git(&mounts, &["ls-files"]).stdout;
        assert!(
            String::from_utf8_lossy(&tracked)
                .lines()
                .any(|line| line == "extra.json")
        );
        assert!(
            reopened
                .registry()
                .get(&Name::new("extra").unwrap())
                .is_some()
        );
    }

    #[test]
    fn observe_reads_mounts_without_initializing_git() {
        let dir = tempfile::tempdir().unwrap();
        let mounts = dir.path().join("mounts");
        fs::create_dir_all(&mounts).unwrap();
        fs::write(
            mounts.join("demo.json"),
            serde_json::to_vec(&spec("demo")).unwrap(),
        )
        .unwrap();

        let repository = Repository::observe(&mounts).unwrap();
        assert!(!mounts.join(".git").exists());
        assert_eq!(repository.head_revision().unwrap(), None);
        assert_eq!(repository.applied().unwrap(), None);
        assert!(
            repository
                .registry()
                .get(&Name::new("demo").unwrap())
                .is_some()
        );
    }

    #[test]
    fn manual_valid_edit_commits_and_applied_is_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let mounts = dir.path().join("mounts");
        let mut repository = Repository::open(&mounts).unwrap();
        repository.put(&spec("demo")).unwrap();
        let first = repository.commit().unwrap();
        let mut edited = spec("demo");
        edited.config_raw = Some(serde_json::json!({"manual": true}));
        fs::write(
            mounts.join("demo.json"),
            serde_json::to_vec_pretty(&edited).unwrap(),
        )
        .unwrap();
        let second = repository.commit().unwrap();
        assert_ne!(first, second);
        assert!(repository.applied().unwrap().is_none());
        repository.mark_applied(&second).unwrap();
        assert_eq!(repository.applied().unwrap(), Some(second.clone()));
        let removed = repository.remove(&Name::new("demo").unwrap()).unwrap();
        assert!(removed);
        let third = repository.commit().unwrap();
        assert_ne!(second, third);
        assert!(repository.registry().iter().next().is_none());
    }

    #[test]
    fn malformed_specs_and_unexpected_tracked_files_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let malformed = dir.path().join("malformed");
        fs::create_dir_all(&malformed).unwrap();
        fs::write(malformed.join("bad.json"), b"{not-json").unwrap();
        assert!(matches!(
            Repository::open(&malformed),
            Err(RepositoryError::InvalidSpecs { .. })
        ));

        let mounts = dir.path().join("tracked");
        let mut repository = Repository::open(&mounts).unwrap();
        repository.put(&spec("demo")).unwrap();
        repository.commit().unwrap();
        fs::write(mounts.join("README"), b"unexpected").unwrap();
        let added = git(&mounts, &["add", "README"]);
        assert!(added.status.success());
        let committed = git(
            &mounts,
            &[
                "-c",
                "user.name=x",
                "-c",
                "user.email=x@y",
                "commit",
                "-m",
                "bad",
            ],
        );
        assert!(committed.status.success());
        drop(repository);
        assert!(matches!(
            Repository::open(&mounts),
            Err(RepositoryError::UnexpectedTracked { .. })
        ));

        let remote_mounts = dir.path().join("remote");
        let remote_repository = Repository::open(&remote_mounts).unwrap();
        drop(remote_repository);
        assert!(
            git(
                &remote_mounts,
                &["remote", "add", "origin", "https://example.invalid/repo"]
            )
            .status
            .success()
        );
        assert!(matches!(
            Repository::open(&remote_mounts),
            Err(RepositoryError::ConfiguredRemote { .. })
        ));
        assert_eq!(
            String::from_utf8_lossy(&git(&remote_mounts, &["remote"]).stdout).trim(),
            "origin"
        );
    }

    #[test]
    fn snapshot_loads_exact_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mounts = dir.path().join("mounts");
        let mut repository = Repository::open(&mounts).unwrap();
        repository.put(&spec("demo")).unwrap();
        let revision = repository.commit().unwrap();
        let cache = dir.path().join("cache");
        let (_, snapshot) = repository.snapshot(&revision, &cache).unwrap();
        let name = Name::new("demo").unwrap();
        assert_eq!(snapshot.get(&name).unwrap().mount, "demo");
        assert!(
            cache
                .join(crate::layout::MOUNT_REVISIONS_SUBDIR)
                .join(revision.as_str())
                .exists()
        );

        let snapshot_path = cache
            .join(crate::layout::MOUNT_REVISIONS_SUBDIR)
            .join(revision.as_str());
        let expected_bytes = fs::read(snapshot_path.join("demo.json")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&snapshot_path, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        fs::write(snapshot_path.join("extra.json"), b"{}").unwrap();
        assert!(matches!(
            repository.snapshot(&revision, &cache),
            Err(RepositoryError::InvalidSnapshot { .. })
        ));
        fs::remove_file(snapshot_path.join("extra.json")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = dir.path().join("outside-demo.json");
            fs::write(&outside, &expected_bytes).unwrap();
            fs::remove_file(snapshot_path.join("demo.json")).unwrap();
            symlink(&outside, snapshot_path.join("demo.json")).unwrap();
            assert!(matches!(
                repository.snapshot(&revision, &cache),
                Err(RepositoryError::InvalidSnapshot { .. })
            ));
            fs::remove_file(snapshot_path.join("demo.json")).unwrap();
            fs::write(snapshot_path.join("demo.json"), expected_bytes).unwrap();
            fs::remove_file(outside).unwrap();
        }

        fs::create_dir_all(mounts.join("nested")).unwrap();
        fs::write(mounts.join("nested/extra.json"), b"{}").unwrap();
        assert!(git(&mounts, &["add", "nested/extra.json"]).status.success());
        let invalid = git(
            &mounts,
            &[
                "-c",
                "user.name=omnifs",
                "-c",
                "user.email=omnifs@localhost",
                "commit",
                "-m",
                "invalid tree",
            ],
        );
        assert!(invalid.status.success());
        let invalid_revision = repository.head_revision().unwrap().unwrap();
        assert!(matches!(
            repository.snapshot(&invalid_revision, &cache),
            Err(RepositoryError::UnexpectedTracked { .. })
        ));
    }
}
