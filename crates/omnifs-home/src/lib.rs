//! omnifs home directory layout and path resolution.
//!
//! This crate is the single source of truth for the omnifs on-disk layout.
//! Both the CLI and daemon depend on it; neither duplicates the resolution
//! logic. The daemon never reads `config.toml`, so the two-pass
//! `resolve_with_config` stays in the CLI.
//!
//! Resolution order (per directory):
//!   1. Explicit override from `PathOverrides` (CLI flag)
//!   2. `OMNIFS_HOME` (fans out: all dirs under `$OMNIFS_HOME`)
//!   3. Default: `$HOME/.omnifs/{...}`

use std::path::{Path, PathBuf};

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

/// Explicit (CLI flag) relocations for individual directories.
#[derive(Debug, Clone, Default)]
pub struct PathOverrides {
    pub config_dir: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
}

/// The fully resolved omnifs directory layout.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Resolve paths from overrides, env, `OMNIFS_HOME`, then the
    /// `$HOME/.omnifs` default.
    ///
    /// Resolution order per directory:
    ///   1. `overrides` field (CLI flag)
    ///   2. `OMNIFS_HOME`
    ///   3. Default root (`$HOME/.omnifs`)
    pub fn resolve(overrides: PathOverrides) -> Result<Self, ResolveError> {
        let omnifs_home = std::env::var_os(OMNIFS_HOME_ENV).map(PathBuf::from);
        let default_root =
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(DEFAULT_HOME_SUBDIR));

        let config_dir = overrides
            .config_dir
            .or_else(|| omnifs_home.clone())
            .or_else(|| default_root.clone())
            .ok_or(ResolveError)?;

        // Start from the canonical flat layout under config_dir, then let
        // per-purpose overrides and env vars relocate individual dirs.
        let mut paths = Self::under_root(&config_dir);

        paths.cache_dir = overrides
            .cache_dir
            .or_else(|| omnifs_home.as_ref().map(|h| h.join(CACHE_SUBDIR)))
            .or_else(|| default_root.map(|root| root.join(CACHE_SUBDIR)))
            .ok_or(ResolveError)?;

        Ok(paths)
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
