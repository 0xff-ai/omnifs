//! omnifs home directory layout and path resolution.
//!
//! This crate is the single source of truth for the omnifs on-disk layout.
//! Lower-level storage code and hermetic fixtures may construct this layout;
//! application code should use [`crate::Workspace`] so component ownership
//! stays behind typed brokers.
//!
//! Resolution order:
//!   1. `OMNIFS_HOME`
//!   2. Default: `$HOME/.omnifs`

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
/// Subdirectory of `cache_dir` holding immutable mount-spec revision snapshots.
pub const MOUNT_REVISIONS_SUBDIR: &str = "mount-revisions";
/// Subdirectory of `cache_dir` holding local frontend state.
pub const FRONTEND_STATE_SUBDIR: &str = "frontends";
/// Subdirectory of `config_dir` holding the daemon's namespace attach sockets
/// served to out-of-process frontend runners: the fixed local socket
/// (`<config_dir>/frontends/local.sock`) and, on demand, the vsock-proxy
/// listener below.
pub const FRONTENDS_SUBDIR: &str = "frontends";
/// Filename of the daemon's fixed namespace attach socket for local frontend
/// runners (`<config_dir>/frontends/local.sock`). One socket, always at this
/// name; auth is filesystem permissions on the socket.
pub const LOCAL_ATTACH_SOCKET_NAME: &str = "local.sock";
/// Filename of the token-checking UDS namespace attach listener under
/// `frontends/` (`<config_dir>/frontends/vsock-attach.sock`). Bound on demand
/// via the daemon's `AttachVsock` control operation, one per daemon instance;
/// unlike the fixed local socket, whose auth is filesystem permissions, a
/// connection here proves itself with a per-instance token, because the
/// libkrun's vsock-proxy path terminates every guest vsock dial on this socket
/// as the same local peer.
pub const VSOCK_ATTACH_SOCKET_NAME: &str = "vsock-attach.sock";
/// Durable token-authenticated TCP and vsock listener authority.
pub const ATTACH_TARGETS_FILE: &str = "targets.json";
pub const OMNIFS_HOME_ENV: &str = "OMNIFS_HOME";
/// Overrides the host-visible mount point the daemon serves at.
pub const OMNIFS_MOUNT_POINT_ENV: &str = "OMNIFS_MOUNT_POINT";

/// The fully resolved omnifs directory layout.
#[derive(Debug)]
pub(crate) struct WorkspaceLayout {
    pub(crate) config_dir: PathBuf,
    pub(crate) cache_dir: PathBuf,
    /// Staging directory holding one JSON file per mount.
    pub(crate) mounts_dir: PathBuf,
    /// Directory holding compiled provider WASM components, looked up
    /// by the `provider:` field of each mount config.
    pub(crate) providers_dir: PathBuf,
    pub(crate) credentials_file: PathBuf,
    pub(crate) config_file: PathBuf,
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
    pub(crate) fn resolve() -> Result<Self, ResolveError> {
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
    pub(crate) fn under_root(root: &Path) -> Self {
        let config_dir = root.to_path_buf();
        WorkspaceLayout {
            config_file: config_dir.join(CONFIG_FILE),
            credentials_file: config_dir.join(CREDENTIALS_FILE),
            mounts_dir: config_dir.join(MOUNTS_SUBDIR),
            providers_dir: config_dir.join(PROVIDERS_SUBDIR),
            cache_dir: config_dir.join(CACHE_SUBDIR),
            config_dir,
        }
    }

    /// Discoverable parent of all local frontend state directories.
    pub(crate) fn frontend_state_root(&self) -> PathBuf {
        self.cache_dir.join(FRONTEND_STATE_SUBDIR)
    }
}

/// Home-relativize a path for human display, falling back to the full path.
pub fn display(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if let Ok(stripped) = path.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
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
