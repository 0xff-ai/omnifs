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

use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use serde::Serialize;

const DEFAULT_HOME_SUBDIR: &str = ".omnifs";

// The on-disk structure of an omnifs root, relative to the root directory.
// Every concrete path (host default resolution and the in-container guest
// layout) is `root` joined with one of these. Host and guest share the same
// flat shape.
pub const CONFIG_FILE: &str = "config.toml";
pub const CREDENTIALS_FILE: &str = "credentials.json";
pub const CONTROL_TOKEN_FILE: &str = "control-token";
pub const MOUNTS_SUBDIR: &str = "mounts";
pub const WORLDVIEWS_SUBDIR: &str = "worldviews";
pub const PROVIDERS_SUBDIR: &str = "providers";
pub const CACHE_SUBDIR: &str = "cache";
/// Subdirectory of `cache_dir` holding NFS loopback mount-state files.
pub const NFS_STATE_SUBDIR: &str = "nfs";
pub const OMNIFS_HOME_ENV: &str = "OMNIFS_HOME";
/// Overrides the host-visible mount point the daemon serves at.
pub const OMNIFS_MOUNT_POINT_ENV: &str = "OMNIFS_MOUNT_POINT";

/// Role marker for code that only needs the shared workspace layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shared;

/// Role marker for daemon-side workspace use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Daemon;

/// Role marker for CLI-side workspace use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cli;

/// A resolved omnifs workspace, parameterized by the capability set using it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace<Role = Shared> {
    layout: WorkspaceLayout,
    _role: PhantomData<Role>,
}

/// The fully resolved omnifs directory layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceLayout {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// Staging directory holding one JSON file per mount.
    pub mounts_dir: PathBuf,
    /// Directory holding named namespace-serving scopes.
    pub worldviews_dir: PathBuf,
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

impl WorkspaceLayout {
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
        WorkspaceLayout {
            config_file: config_dir.join(CONFIG_FILE),
            credentials_file: config_dir.join(CREDENTIALS_FILE),
            mounts_dir: config_dir.join(MOUNTS_SUBDIR),
            worldviews_dir: config_dir.join(WORLDVIEWS_SUBDIR),
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

    /// Bearer token file for the daemon control port (`<config_dir>/control-token`).
    pub fn control_token_file(&self) -> PathBuf {
        self.config_dir.join(CONTROL_TOKEN_FILE)
    }

    pub fn provider_path(&self, provider: &str) -> PathBuf {
        let provider = PathBuf::from(provider);
        if provider.is_absolute() {
            provider
        } else {
            self.providers_dir.join(provider)
        }
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

/// Compiled provider-component artifacts live under `<cache_dir>/wasm`, with
/// the rest of the host's state, rather than a global per-user wasmtime cache.
/// The single owner of this path: both the resolved workspace layout and the
/// host runtime derive it from their cache dir through here.
pub fn wasm_cache_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("wasm")
}

/// Resolve the host-visible mount point the daemon serves at:
/// `OMNIFS_MOUNT_POINT` when set (the container entrypoint exports it),
/// otherwise `$HOME/omnifs`, deliberately outside `OMNIFS_HOME` so the mounted
/// tree lives at a normal user-owned location. `None` only when neither is
/// available.
///
/// Single owner of this path: the daemon serves here and `omnifs setup`
/// previews it, so the served location and the preview cannot drift.
pub fn resolve_mount_point() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(OMNIFS_MOUNT_POINT_ENV) {
        return Some(PathBuf::from(explicit));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("omnifs"))
}

impl<Role> Workspace<Role> {
    /// Resolve a role-specific workspace from env, `OMNIFS_HOME`, then the
    /// `$HOME/.omnifs` default.
    pub fn resolve() -> Result<Self, ResolveError> {
        Ok(Self::from_layout(WorkspaceLayout::resolve()?))
    }

    /// Assemble a role-specific workspace under a single root.
    pub fn under_root(root: &Path) -> Self {
        Self::from_layout(WorkspaceLayout::under_root(root))
    }

    /// Wrap an already-resolved layout.
    pub fn from_layout(layout: WorkspaceLayout) -> Self {
        Self {
            layout,
            _role: PhantomData,
        }
    }

    pub fn layout(&self) -> &WorkspaceLayout {
        &self.layout
    }

    pub fn into_layout(self) -> WorkspaceLayout {
        self.layout
    }

    pub fn config_dir(&self) -> &Path {
        &self.layout.config_dir
    }

    pub fn cache_dir(&self) -> &Path {
        &self.layout.cache_dir
    }

    pub fn mounts_dir(&self) -> &Path {
        &self.layout.mounts_dir
    }

    pub fn worldviews_dir(&self) -> &Path {
        &self.layout.worldviews_dir
    }

    pub fn providers_dir(&self) -> &Path {
        &self.layout.providers_dir
    }

    pub fn credentials_file(&self) -> &Path {
        &self.layout.credentials_file
    }

    pub fn config_file(&self) -> &Path {
        &self.layout.config_file
    }

    pub fn nfs_state_dir(&self) -> PathBuf {
        self.layout.nfs_state_dir()
    }

    pub fn control_token_file(&self) -> PathBuf {
        self.layout.control_token_file()
    }

    pub fn display_path(&self, path: &Path) -> String {
        WorkspaceLayout::display(path)
    }
}
