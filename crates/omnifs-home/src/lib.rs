//! omnifs home directory layout and path resolution.
//!
//! This crate is the single source of truth for the omnifs on-disk layout.
//! Both the CLI and daemon depend on it; neither duplicates the resolution
//! logic. Higher-level CLI factories layer config and daemon handles on top of
//! this path-only layout.
//!
//! Resolution order:
//!   1. `OMNIFS_HOME`
//!   2. Default: `$HOME/.omnifs`

use std::path::{Path, PathBuf};

use serde::Serialize;

const DEFAULT_HOME_SUBDIR: &str = ".omnifs";

// The on-disk structure of an omnifs root, relative to the root directory.
// Every concrete path (host default resolution and the in-container guest
// layout) is `root` joined with one of these. Host and guest share the same
// flat shape.
pub const CONFIG_FILE: &str = "config.toml";
pub const CREDENTIALS_FILE: &str = "credentials.json";
pub const MOUNTS_SUBDIR: &str = "mounts";
pub const PROVIDERS_SUBDIR: &str = "providers";
pub const CACHE_SUBDIR: &str = "cache";
/// Subdirectory of `cache_dir` holding NFS loopback mount-state files.
pub const NFS_STATE_SUBDIR: &str = "nfs";
pub const OMNIFS_HOME_ENV: &str = "OMNIFS_HOME";

/// The fully resolved omnifs directory layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// Staging directory holding one JSON file per mount.
    pub mounts_dir: PathBuf,
    /// Directory holding compiled provider WASM components, looked up
    /// by the `provider:` field of each mount config.
    pub providers_dir: PathBuf,
    pub credentials_file: PathBuf,
    pub config_file: PathBuf,
}

/// Path resolution failed because no default root could be derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveError;

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cannot resolve omnifs home: set HOME or OMNIFS_HOME")
    }
}

impl std::error::Error for ResolveError {}

impl Paths {
    /// Resolve paths from env, `OMNIFS_HOME`, then the `$HOME/.omnifs` default.
    pub fn resolve() -> Result<Self, ResolveError> {
        let omnifs_home = std::env::var_os(OMNIFS_HOME_ENV).map(PathBuf::from);
        let default_root =
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(DEFAULT_HOME_SUBDIR));

        let root = omnifs_home.or(default_root).ok_or(ResolveError)?;

        Ok(Self::under_root(&root))
    }

    /// Assemble the canonical flat layout under a single `root`.
    ///
    /// This is the one place that maps the omnifs structure to concrete paths.
    /// Both host default resolution and the in-container guest layout build on
    /// this so they always stay in sync.
    pub fn under_root(root: &Path) -> Self {
        let config_dir = root.to_path_buf();
        Paths {
            config_file: config_dir.join(CONFIG_FILE),
            credentials_file: config_dir.join(CREDENTIALS_FILE),
            mounts_dir: config_dir.join(MOUNTS_SUBDIR),
            providers_dir: config_dir.join(PROVIDERS_SUBDIR),
            cache_dir: config_dir.join(CACHE_SUBDIR),
            config_dir,
        }
    }

    /// Directory holding NFS loopback mount-state files (`<cache_dir>/nfs`).
    ///
    /// Single source of this path: the daemon writes state files here (its
    /// `--nfs-state-dir` default) and the CLI reads them for host-native
    /// `omnifs down`, so the producer and consumer cannot drift.
    pub fn nfs_state_dir(&self) -> PathBuf {
        self.cache_dir.join(NFS_STATE_SUBDIR)
    }

    /// Home-relativize a path for display (e.g. `~/.omnifs/config.toml`).
    /// Falls back to the full path if HOME is unset or stripping fails.
    pub fn display(path: &Path) -> String {
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            if let Ok(stripped) = path.strip_prefix(&home) {
                return format!("~/{}", stripped.display());
            }
        }
        path.display().to_string()
    }
}
