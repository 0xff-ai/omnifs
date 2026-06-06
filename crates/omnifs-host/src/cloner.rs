//! Git repository cloning with coalescing and timeout.
//!
//! `GitCloner` manages shallow clones of git repositories with
//! locking to prevent concurrent clones of the same repo.
//! Uses --depth=1 --single-branch --no-tags for fast first access;
//! the FUSE passthrough only reads the HEAD working tree.

use crate::sandbox::publish;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::io::Read as _;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;

const CLONE_TIMEOUT: Duration = Duration::from_secs(120);
const STDERR_MAX_BYTES: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum CloneError {
    #[error("clone failed (exit {status}): {stderr}")]
    Failed { status: ExitStatus, stderr: String },
    #[error("clone timed out after {timeout_secs}s")]
    Timeout { timeout_secs: u64 },
    #[error("failed to spawn git")]
    Spawn(#[from] std::io::Error),
    #[error("cache key conflict: expected {expected}, found {found}")]
    CacheKeyConflict { expected: String, found: String },
    #[error("unsafe cache key: {0}")]
    UnsafeCacheKey(String),
}

/// Validate that a cache key is safe to use as a relative path.
/// Rejects: absolute paths, .. components, . components, empty components
/// (double //), NUL bytes, and platform path separators other than /.
fn is_safe_cache_key(key: &str) -> bool {
    if key.is_empty() || key.starts_with('/') {
        return false;
    }
    if key.bytes().any(|b| b == 0) {
        return false;
    }
    for component in key.split('/') {
        if component.is_empty() || component == ".." || component == "." {
            return false;
        }
        if component.contains(MAIN_SEPARATOR) && MAIN_SEPARATOR != '/' {
            return false;
        }
    }
    true
}

/// Observer notified when a clone actually executes (cache miss). The
/// runtime hooks this up to the inspector stream; tests and
/// non-instrumented callers pass [`NoopCloneObserver`].
pub trait CloneObserver {
    /// A fresh clone is about to begin.
    fn on_clone_start(&mut self, cache_key: &str, clone_url: &str);
    /// The clone has finished (success or failure).
    fn on_clone_end(&mut self, cache_key: &str, elapsed: Duration, ok: bool);
}

/// Observer that drops every notification. Used when no live sink is
/// installed (tests, unit checks, etc.).
pub struct NoopCloneObserver;

impl CloneObserver for NoopCloneObserver {
    fn on_clone_start(&mut self, _cache_key: &str, _clone_url: &str) {}
    fn on_clone_end(&mut self, _cache_key: &str, _elapsed: Duration, _ok: bool) {}
}

/// Shared clone infrastructure. Owns the cache directory and a lock map
/// to coalesce concurrent clones of the same repository.
pub struct GitCloner {
    cache_dir: PathBuf,
    locks: DashMap<String, Arc<Mutex<()>>>,
}

impl GitCloner {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            locks: DashMap::new(),
        }
    }

    /// Return the local cache path for a repository, cloning if needed.
    /// `cache_key` is a provider-supplied stable identifier (e.g. "github.com/owner/repo").
    /// `clone_url` is the full URL to pass to git clone verbatim.
    ///
    /// `observer` is notified only when a fresh clone is about to run
    /// (cache miss). Cache hits never call the observer.
    pub fn clone_if_needed(
        &self,
        cache_key: &str,
        clone_url: &str,
        observer: &mut dyn CloneObserver,
    ) -> Result<PathBuf, CloneError> {
        if !is_safe_cache_key(cache_key) {
            return Err(CloneError::UnsafeCacheKey(cache_key.to_string()));
        }

        let cache_path = self.cache_dir.join(cache_key);

        let lock = self
            .locks
            .entry(cache_key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock();

        // Check sidecar binding: if a .omnifs-clone-url file exists,
        // verify it matches the requested clone_url.
        let sidecar = cache_path.join(".omnifs-clone-url");
        if cache_path.join(".git").is_dir() {
            if let Ok(recorded) = std::fs::read_to_string(&sidecar) {
                let recorded = recorded.trim();
                if recorded != clone_url {
                    return Err(CloneError::CacheKeyConflict {
                        expected: clone_url.to_string(),
                        found: recorded.to_string(),
                    });
                }
            } else {
                // Adopt existing cache entry: write sidecar for first time.
                Self::write_sidecar(&sidecar, clone_url);
            }
            return Ok(cache_path);
        }

        observer.on_clone_start(cache_key, clone_url);
        let started = Instant::now();
        let outcome = Self::run_clone(clone_url, &cache_path);
        observer.on_clone_end(cache_key, started.elapsed(), outcome.is_ok());
        outcome?;
        Self::write_sidecar(&sidecar, clone_url);
        Ok(cache_path)
    }

    /// Return the cache directory root.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    fn run_clone(url: &str, dest: &Path) -> Result<(), CloneError> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut child = Command::new("git")
            .args(["clone", "--depth=1", "--single-branch", "--no-tags", url])
            .arg(dest)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;

        // Drain stderr on a separate thread to avoid pipe-full deadlock.
        // Keep reading past the cap (to prevent pipe backup) but only
        // retain the first STDERR_MAX_BYTES.
        let stderr_handle = child.stderr.take();
        let stderr_thread = std::thread::spawn(move || {
            let mut retained = Vec::with_capacity(STDERR_MAX_BYTES);
            let mut discard = [0u8; 1024];
            if let Some(mut pipe) = stderr_handle {
                loop {
                    match pipe.read(&mut discard) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let remaining = STDERR_MAX_BYTES.saturating_sub(retained.len());
                            if remaining > 0 {
                                retained.extend_from_slice(&discard[..n.min(remaining)]);
                            }
                            // Keep reading to drain the pipe even after cap.
                        },
                    }
                }
            }
            String::from_utf8_lossy(&retained).to_string()
        });

        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let stderr = stderr_thread.join().unwrap_or_default();
                    if status.success() {
                        return Ok(());
                    }
                    let _ = std::fs::remove_dir_all(dest);
                    warn!(url, %status, stderr = %stderr, "git clone failed");
                    return Err(CloneError::Failed { status, stderr });
                },
                Ok(None) => {
                    if start.elapsed() > CLONE_TIMEOUT {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = std::fs::remove_dir_all(dest);
                        warn!(url, "git clone timed out");
                        return Err(CloneError::Timeout {
                            timeout_secs: CLONE_TIMEOUT.as_secs(),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(500));
                },
                Err(e) => {
                    let _ = std::fs::remove_dir_all(dest);
                    return Err(CloneError::Spawn(e));
                },
            }
        }
    }

    /// Write the clone URL to a sidecar file. Best-effort; the cache
    /// entry is still usable without the sidecar (we just lose the
    /// clone-url conflict check on the next open).
    fn write_sidecar(path: &Path, clone_url: &str) {
        let _ = publish::replace_file_via_temp_rename(path, clone_url.as_bytes());
    }
}
