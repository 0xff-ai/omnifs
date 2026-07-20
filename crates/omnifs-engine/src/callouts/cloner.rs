//! Git repository cloning with host-owned opaque identities.

use crate::cache::{canonical_directory, ensure_directory, identity::GitId};
use crate::log_redaction::LogUrl;
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::warn;
use url::Url;

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
    #[error("failed to wait for git")]
    Wait(#[source] std::io::Error),
    #[error("invalid git reference")]
    InvalidReference,
    #[error("invalid git remote")]
    InvalidRemote,
    #[error("existing clone identity is unavailable")]
    ExistingEntry,
    #[error("failed to publish clone")]
    Publish(#[source] std::io::Error),
    #[error("git cache I/O failed")]
    Cache(#[source] std::io::Error),
}

/// Shared clone infrastructure rooted at a dedicated host-owned directory.
pub struct GitCloner {
    cache_dir: PathBuf,
    locks: DashMap<String, Arc<Mutex<()>>>,
}

impl GitCloner {
    pub fn new(cache_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let cache_dir = canonical_directory(cache_dir.as_ref())?;
        ensure_directory(&cache_dir)?;
        Ok(Self {
            cache_dir,
            locks: DashMap::new(),
        })
    }

    /// Open the existing clone cache root without creating or sweeping it.
    pub fn open_existing(cache_dir: impl AsRef<Path>) -> Result<Self, CloneError> {
        let cache_dir =
            crate::cache::existing_directory(cache_dir.as_ref()).map_err(CloneError::Cache)?;
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
            || reference.split('/').any(|part| {
                part.is_empty()
                    || part.starts_with('.')
                    || Path::new(part).extension().is_some_and(|ext| ext == "lock")
            })
        {
            return Err(CloneError::InvalidReference);
        }
        Ok(())
    }

    pub(crate) fn canonical_remote(raw: &str) -> Result<String, CloneError> {
        let remote = raw.trim();
        if remote.is_empty()
            || remote
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte == 0)
        {
            return Err(CloneError::InvalidRemote);
        }
        if let Ok(mut url) = Url::parse(remote) {
            if !matches!(url.scheme(), "https" | "ssh" | "git") || url.host_str().is_none() {
                return Err(CloneError::InvalidRemote);
            }
            if url.scheme() == "https" || url.scheme() == "git" {
                url.set_username("")
                    .map_err(|()| CloneError::InvalidRemote)?;
            }
            url.set_password(None)
                .map_err(|()| CloneError::InvalidRemote)?;
            return Ok(url.to_string());
        }

        let (user_host, path) = remote.split_once(':').ok_or(CloneError::InvalidRemote)?;
        let (username, host) = user_host
            .rsplit_once('@')
            .map_or((None, user_host), |(username, host)| (Some(username), host));
        if host.is_empty() || path.is_empty() || path.starts_with('/') {
            return Err(CloneError::InvalidRemote);
        }
        Ok(match username {
            Some(username) => format!("{username}@{host}:{path}"),
            None => format!("{host}:{path}"),
        })
    }

    /// Reopen and validate an existing mount-scoped clone without invoking Git
    /// or consulting the network. `relative_path` is validated beneath the
    /// repository root, and the returned path is that confined selected
    /// directory.
    pub(crate) fn open_cached(
        &self,
        mount_scope: &str,
        id: &GitId,
        relative_path: &str,
    ) -> Result<PathBuf, CloneError> {
        crate::cache::existing_directory(&self.cache_dir).map_err(CloneError::Cache)?;
        let wrapper = self.cache_dir.join(id.filesystem_name());
        let repo = wrapper.join(CLONE_REPO_DIR);
        validate_owned_directory(&wrapper)?;
        validate_owned_directory(&repo)?;
        validate_owned_directory(&repo.join(".git"))?;
        let binding = Self::read_binding(&Self::binding_path(&wrapper))?;
        let canonical = Self::canonical_remote(&binding.remote)?;
        if canonical != binding.remote {
            return Err(CloneError::ExistingEntry);
        }
        if let Some(reference) = binding.reference.as_deref() {
            Self::validate_reference(reference)?;
        }
        if GitId::new(mount_scope, &canonical, binding.reference.as_deref()) != *id {
            return Err(CloneError::ExistingEntry);
        }
        validate_relative_selection(&repo, relative_path)
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
        ensure_directory(&self.cache_dir).map_err(CloneError::Cache)?;
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Ok(_) | Err(_) => return Err(CloneError::ExistingEntry),
        }

        let span = crate::inspector::clone_span(operation_id, &cache_id, clone_url);
        let temporary = Self::temp_sibling_path(&cache_path);
        let temporary_repo = temporary.join(CLONE_REPO_DIR);
        std::fs::create_dir(&temporary).map_err(CloneError::Publish)?;
        let outcome = span.in_scope(|| {
            Self::run_clone(clone_url, reference, &temporary_repo).and_then(|()| {
                Self::write_binding(&Self::binding_path(&temporary), canonical_remote, reference)
                    .map_err(CloneError::Publish)?;
                Self::publish_dir_by_rename(&temporary, &cache_path).map_err(CloneError::Publish)
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
            Self::remove_path_best_effort(&temporary);
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
        Self::replace_file_via_temp_rename(path, &bytes)
    }

    /// Unique sibling temp path for a later rename into `dest`.
    ///
    /// Visibility is atomic on one filesystem via rename; this does not fsync
    /// and does not claim crash durability.
    fn temp_sibling_path(dest: &Path) -> PathBuf {
        let name = dest
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        dest.with_file_name(format!(".{name}.tmp.{}.{nanos}", std::process::id()))
    }

    fn publish_dir_by_rename(source: &Path, destination: &Path) -> std::io::Result<()> {
        let source_meta = std::fs::symlink_metadata(source)?;
        if !source_meta.is_dir() || source_meta.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "publish source is not a regular directory",
            ));
        }
        if let Some(parent) = destination.parent() {
            let metadata = std::fs::symlink_metadata(parent)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "publish destination parent is not a regular directory",
                ));
            }
        }
        std::fs::rename(source, destination)
    }

    fn replace_file_via_temp_rename(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let tmp = Self::temp_sibling_path(path);
        if let Err(e) = std::fs::write(&tmp, bytes) {
            Self::remove_path_best_effort(&tmp);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            Self::remove_path_best_effort(&tmp);
            return Err(e);
        }
        Ok(())
    }

    fn remove_existing_path(path: &Path) -> std::io::Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        }
    }

    fn remove_path_best_effort(path: &Path) {
        let _ = Self::remove_existing_path(path);
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
        let Ok(binding) = Self::read_binding(&Self::binding_path(path)) else {
            return false;
        };
        binding.remote == remote && binding.reference.as_deref() == reference
    }

    fn read_binding(path: &Path) -> Result<CloneBinding, CloneError> {
        let metadata = std::fs::symlink_metadata(path).map_err(CloneError::Cache)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(CloneError::ExistingEntry);
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(path).map_err(CloneError::Cache)?;
        let metadata = file.metadata().map_err(CloneError::Cache)?;
        if !metadata.is_file() {
            return Err(CloneError::ExistingEntry);
        }
        let capacity = usize::try_from(metadata.len()).map_err(|_| CloneError::ExistingEntry)?;
        let mut raw = Vec::with_capacity(capacity);
        file.read_to_end(&mut raw).map_err(CloneError::Cache)?;
        serde_json::from_slice(&raw).map_err(|_| CloneError::ExistingEntry)
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
                    Self::remove_path_best_effort(dest);
                    warn!(url = %LogUrl(url), %status, "git clone failed");
                    return Err(CloneError::Failed { status });
                },
                Ok(None) if start.elapsed() > CLONE_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stderr_thread.join();
                    Self::remove_path_best_effort(dest);
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
                    Self::remove_path_best_effort(dest);
                    return Err(CloneError::Wait(error));
                },
            }
        }
    }
}

fn validate_owned_directory(path: &Path) -> Result<(), CloneError> {
    let metadata = std::fs::symlink_metadata(path).map_err(CloneError::Cache)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CloneError::ExistingEntry);
    }
    Ok(())
}

fn validate_relative_selection(root: &Path, relative: &str) -> Result<PathBuf, CloneError> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
                    | std::path::Component::ParentDir
                    | std::path::Component::CurDir
            )
        })
    {
        return Err(CloneError::ExistingEntry);
    }
    let mut selected = root.to_path_buf();
    for component in relative.components() {
        selected.push(component.as_os_str());
        validate_owned_directory(&selected)?;
    }
    Ok(selected)
}

#[derive(Debug, Serialize, Deserialize)]
struct CloneBinding {
    remote: String,
    reference: Option<String>,
}

#[cfg(all(test, unix))]
mod tests {
    use super::{CLONE_REPO_DIR, GitCloner};

    #[test]
    fn clone_validation_rejects_symlinked_wrapper_and_repo() {
        let temp = tempfile::tempdir().unwrap();
        let cloner = GitCloner::new(temp.path().join("clones")).unwrap();
        let root = &cloner.cache_dir;
        let remote = "https://example.test/repo.git";
        let reference = Some("main");

        let wrapper_target = root.join("wrapper-target");
        std::fs::create_dir_all(wrapper_target.join(CLONE_REPO_DIR).join(".git")).unwrap();
        GitCloner::write_binding(&GitCloner::binding_path(&wrapper_target), remote, reference)
            .unwrap();
        let wrapper_link = root.join("wrapper-link");
        std::os::unix::fs::symlink(&wrapper_target, &wrapper_link).unwrap();
        assert!(!GitCloner::is_valid_clone(&wrapper_link, remote, reference));

        let repo_target = root.join("repo-target");
        std::fs::create_dir_all(repo_target.join(".git")).unwrap();
        let wrapper = root.join("wrapper");
        std::fs::create_dir(&wrapper).unwrap();
        std::os::unix::fs::symlink(&repo_target, wrapper.join(CLONE_REPO_DIR)).unwrap();
        GitCloner::write_binding(&GitCloner::binding_path(&wrapper), remote, reference).unwrap();
        assert!(!GitCloner::is_valid_clone(&wrapper, remote, reference));
    }
}
