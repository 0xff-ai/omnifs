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
/// via `POST /v1/frontend/attach-target/vsock`, one per daemon instance;
/// unlike the fixed local socket, whose auth is filesystem permissions, a
/// connection here proves itself with a per-instance token, because the
/// krunkit vsock-proxy path terminates every guest vsock dial on this socket
/// as the same local peer.
pub const VSOCK_ATTACH_SOCKET_NAME: &str = "vsock-attach.sock";
pub const OMNIFS_HOME_ENV: &str = "OMNIFS_HOME";
/// Overrides the host-visible mount point the daemon serves at.
pub const OMNIFS_MOUNT_POINT_ENV: &str = "OMNIFS_MOUNT_POINT";

/// Role marker for code that only needs the shared workspace layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shared;

/// Role marker for daemon-side workspace use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Daemon;

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
            providers_dir: config_dir.join(PROVIDERS_SUBDIR),
            cache_dir: config_dir.join(CACHE_SUBDIR),
            config_dir,
        }
    }

    /// Discoverable parent of all local frontend state directories.
    pub fn frontend_state_root(&self) -> PathBuf {
        self.cache_dir.join(FRONTEND_STATE_SUBDIR)
    }

    /// Root of immutable mount desired-state snapshots keyed by Git revision.
    pub fn mount_revisions_root(&self) -> PathBuf {
        self.cache_dir.join(MOUNT_REVISIONS_SUBDIR)
    }

    /// Stable state directory for one local frontend mount.
    ///
    /// Each leaf owns its runner discovery record and, for NFS, its persistent
    /// filehandle table. Isolating leaves prevents two NFS mounts from sharing
    /// protocol identity.
    pub fn frontend_state_dir(
        &self,
        kind: crate::runtime_record::FrontendKind,
        mount_point: &Path,
    ) -> PathBuf {
        let normalized = mount_point.components().collect::<PathBuf>();
        let digest = blake3::hash(normalized.as_os_str().as_encoded_bytes()).to_hex();
        self.frontend_state_root()
            .join(kind.label())
            .join(digest.as_str())
    }

    /// The daemon-owned runtime record (`<config_dir>/daemon.json`). The daemon
    /// writes it on start and removes it on graceful exit; the CLI reads it to
    /// resolve which endpoint to dial.
    pub fn runtime_record_file(&self) -> PathBuf {
        self.config_dir
            .join(crate::runtime_record::RUNTIME_RECORD_FILE)
    }

    /// The host-native control socket (`<config_dir>/control.sock`). Auth on
    /// this socket is filesystem permissions, not a bearer token.
    pub fn control_socket(&self) -> PathBuf {
        self.config_dir
            .join(crate::runtime_record::CONTROL_SOCKET_FILE)
    }

    /// Directory holding the daemon's namespace attach sockets
    /// (`<config_dir>/frontends`). The daemon creates it `0700` when it binds an
    /// attach socket.
    pub fn frontends_dir(&self) -> PathBuf {
        self.config_dir.join(FRONTENDS_SUBDIR)
    }

    /// The daemon's fixed namespace attach socket for local frontend runners
    /// (`<config_dir>/frontends/local.sock`). Auth on the socket is
    /// filesystem permissions.
    pub fn local_attach_socket(&self) -> PathBuf {
        self.frontends_dir().join(LOCAL_ATTACH_SOCKET_NAME)
    }

    /// The token-checking UDS namespace attach listener
    /// (`<config_dir>/frontends/vsock-attach.sock`). See
    /// [`VSOCK_ATTACH_SOCKET_NAME`].
    pub fn vsock_attach_socket(&self) -> PathBuf {
        self.frontends_dir().join(VSOCK_ATTACH_SOCKET_NAME)
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

    pub fn mounts_dir(&self) -> &Path {
        &self.layout.mounts_dir
    }

    pub fn providers_dir(&self) -> &Path {
        &self.layout.providers_dir
    }

    pub fn config_file(&self) -> &Path {
        &self.layout.config_file
    }
}
