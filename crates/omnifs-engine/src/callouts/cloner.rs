//! Git repository cloning with host-owned opaque identities.

use crate::cache::identity::GitId;
use crate::log_redaction::LogUrl;
use crate::sandbox::publish;
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;

const CLONE_TIMEOUT: Duration = Duration::from_mins(2);
const CLONE_REPO_DIR: &str = "repo";
const CLONE_BINDING_FILE: &str = "binding.json";

#[derive(Debug, thiserror::Error)]
pub enum CloneError {
    #[error("clone failed (exit {status})")]
    Failed { status: ExitStatus },
    #[error("clone timed out after {timeout_secs}s")]
    Timeout { timeout_secs: u64 },
    #[error("failed to spawn git")]
    Spawn(#[from] std::io::Error),
    #[error("invalid git reference")]
    InvalidReference,
    #[error("existing clone identity is unavailable")]
    ExistingEntry,
    #[error("failed to publish clone")]
    Publish,
}

/// Shared clone infrastructure rooted at a dedicated host-owned directory.
pub struct GitCloner {
    cache_dir: PathBuf,
    locks: DashMap<String, Arc<Mutex<()>>>,
}

impl GitCloner {
    pub fn new(cache_dir: PathBuf) -> std::io::Result<Self> {
        ensure_directory(&cache_dir)?;
        Ok(Self {
            cache_dir,
            locks: DashMap::new(),
        })
    }

    pub(crate) fn validate_reference(reference: &str) -> Result<(), CloneError> {
        if reference.is_empty()
            || reference.starts_with('-')
            || reference.starts_with('/')
            || reference.ends_with('/')
            || reference.ends_with('.')
            || reference == "@"
            || reference.contains("..")
            || reference.contains("@{")
            || reference
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
            || reference
                .chars()
                .any(|ch| matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
            || reference
                .split('/')
                .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
        {
            return Err(CloneError::InvalidReference);
        }
        Ok(())
    }

    /// Return the local cache path for a host-derived identity, cloning if needed.
    pub(crate) fn clone_if_needed(
        &self,
        id: &GitId,
        clone_url: &str,
        canonical_remote: &str,
        reference: Option<&str>,
        operation_id: u64,
    ) -> Result<PathBuf, CloneError> {
        ensure_directory(&self.cache_dir).map_err(|_| CloneError::ExistingEntry)?;
        let cache_id = id.to_string();
        let cache_path = self.cache_dir.join(id.filesystem_name());
        let lock = self
            .locks
            .entry(cache_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock();

        match std::fs::symlink_metadata(&cache_path) {
            Ok(_) if Self::is_valid_clone(&cache_path, canonical_remote, reference) => {
                return Ok(cache_path.join(CLONE_REPO_DIR));
            },
            Ok(_) => return Err(CloneError::ExistingEntry),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(_) => return Err(CloneError::ExistingEntry),
        }

        let span = crate::inspector::clone_span(operation_id, &cache_id, clone_url);
        let temporary = publish::temp_sibling_path(&cache_path);
        let temporary_repo = temporary.join(CLONE_REPO_DIR);
        std::fs::create_dir(&temporary).map_err(|_| CloneError::Publish)?;
        let outcome = span.in_scope(|| {
            Self::run_clone(clone_url, reference, &temporary_repo).and_then(|()| {
                Self::write_binding(&Self::binding_path(&temporary), canonical_remote, reference)?;
                publish::publish_dir_by_rename(&temporary, &cache_path)
                    .map_err(|_| CloneError::Publish)
            })
        });
        crate::inspector::record_outcome(
            &span,
            if outcome.is_ok() {
                omnifs_api::events::InspectorOutcome::Ok
            } else {
                omnifs_api::events::InspectorOutcome::Network
            },
        );
        if let Err(error) = outcome {
            publish::remove_path_best_effort(&temporary);
            return Err(error);
        }

        Ok(cache_path.join(CLONE_REPO_DIR))
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    fn binding_path(path: &Path) -> PathBuf {
        path.join(CLONE_BINDING_FILE)
    }

    fn write_binding(path: &Path, remote: &str, reference: Option<&str>) -> std::io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("clone metadata has no parent"))?;
        ensure_directory(parent)
            .map_err(|_| std::io::Error::other("clone metadata root unavailable"))?;
        let binding = CloneBinding {
            remote: remote.to_string(),
            reference: reference.map(ToOwned::to_owned),
        };
        let bytes = serde_json::to_vec(&binding)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        publish::replace_file_via_temp_rename(path, &bytes)
    }

    fn is_valid_clone(path: &Path, remote: &str, reference: Option<&str>) -> bool {
        let wrapper = std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
        let repo = std::fs::symlink_metadata(path.join(CLONE_REPO_DIR))
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
        let git = std::fs::symlink_metadata(path.join(CLONE_REPO_DIR).join(".git"))
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            && std::fs::symlink_metadata(Self::binding_path(path))
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
        if !(wrapper && repo && git) {
            return false;
        }
        let Ok(raw) = std::fs::read_to_string(Self::binding_path(path)) else {
            return false;
        };
        let Ok(binding) = serde_json::from_str::<CloneBinding>(&raw) else {
            return false;
        };
        binding.remote == remote && binding.reference.as_deref() == reference
    }

    fn run_clone(url: &str, reference: Option<&str>, dest: &Path) -> Result<(), CloneError> {
        let mut command = Command::new("git");
        command.args(["clone", "--depth=1", "--single-branch", "--no-tags"]);
        if let Some(reference) = reference {
            command.args(["--branch", reference]);
        }
        let mut child = command
            .arg("--")
            .arg(url)
            .arg(dest)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr_handle = child.stderr.take();
        let stderr_thread = std::thread::spawn(move || {
            if let Some(mut pipe) = stderr_handle {
                let mut discard = [0u8; 1024];
                loop {
                    match pipe.read(&mut discard) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {},
                    }
                }
            }
        });
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = stderr_thread.join();
                    if status.success() {
                        return Ok(());
                    }
                    publish::remove_path_best_effort(dest);
                    warn!(url = %LogUrl(url), %status, "git clone failed");
                    return Err(CloneError::Failed { status });
                },
                Ok(None) if start.elapsed() > CLONE_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stderr_thread.join();
                    publish::remove_path_best_effort(dest);
                    warn!(url = %LogUrl(url), "git clone timed out");
                    return Err(CloneError::Timeout {
                        timeout_secs: CLONE_TIMEOUT.as_secs(),
                    });
                },
                Ok(None) => std::thread::sleep(Duration::from_millis(500)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stderr_thread.join();
                    publish::remove_path_best_effort(dest);
                    return Err(CloneError::Spawn(error));
                },
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CloneBinding {
    remote: String,
    reference: Option<String>,
}

fn ensure_directory(path: &Path) -> std::io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {},
            Ok(_) => return Err(std::io::Error::other("path is not an owned directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?;
            },
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::{CLONE_REPO_DIR, GitCloner};

    #[test]
    fn clone_validation_rejects_symlinked_wrapper_and_repo() {
        let temp = tempfile::tempdir().unwrap();
        let remote = "https://example.test/repo.git";
        let reference = Some("main");

        let wrapper_target = temp.path().join("wrapper-target");
        std::fs::create_dir_all(wrapper_target.join(CLONE_REPO_DIR).join(".git")).unwrap();
        GitCloner::write_binding(&GitCloner::binding_path(&wrapper_target), remote, reference)
            .unwrap();
        let wrapper_link = temp.path().join("wrapper-link");
        std::os::unix::fs::symlink(&wrapper_target, &wrapper_link).unwrap();
        assert!(!GitCloner::is_valid_clone(&wrapper_link, remote, reference));

        let repo_target = temp.path().join("repo-target");
        std::fs::create_dir_all(repo_target.join(".git")).unwrap();
        let wrapper = temp.path().join("wrapper");
        std::fs::create_dir(&wrapper).unwrap();
        std::os::unix::fs::symlink(&repo_target, wrapper.join(CLONE_REPO_DIR)).unwrap();
        GitCloner::write_binding(&GitCloner::binding_path(&wrapper), remote, reference).unwrap();
        assert!(!GitCloner::is_valid_clone(&wrapper, remote, reference));
    }
}
