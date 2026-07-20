//! The workspace broker for persistent omnifs state.
//!
//! Relative names under the home, home-root env resolution, and the free
//! helpers that are not component fields live here alongside [`Workspace`].
//! Resolution order for the home root:
//!   1. `OMNIFS_HOME`
//!   2. Default: `$HOME/.omnifs`

use std::cell::{OnceCell, RefCell, RefMut};
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::creds::FileStore;
use crate::mounts::{Name, Registry, Repository, Revision, Spec, SpecError};
use crate::provider::Catalog;
use atomic_write_file::OpenOptions as AtomicOpenOptions;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

const DEFAULT_HOME_SUBDIR: &str = ".omnifs";

// The on-disk structure of an omnifs root, relative to the root directory.
// Every concrete path (host default resolution and the in-container guest
// layout) is `root` joined with one of these. Host and guest share the same
// flat shape.
pub(crate) const CONFIG_FILE: &str = "config.toml";
pub(crate) const CREDENTIALS_FILE: &str = "credentials.json";
pub(crate) const MOUNTS_SUBDIR: &str = "mounts";
pub(crate) const PROVIDERS_SUBDIR: &str = "providers";
pub(crate) const CACHE_SUBDIR: &str = "cache";
/// Subdirectory of `cache_dir` holding immutable mount-spec revision snapshots.
pub(crate) const MOUNT_REVISIONS_SUBDIR: &str = "mount-revisions";
/// Subdirectory of `cache_dir` holding local frontend state.
pub(crate) const FRONTEND_STATE_SUBDIR: &str = "frontends";
/// Subdirectory of `config_dir` holding the daemon's namespace attach sockets
/// served to out-of-process frontend runners: the fixed local socket
/// (`<config_dir>/frontends/local.sock`) and, on demand, the vsock-proxy
/// listener below.
pub(crate) const FRONTENDS_SUBDIR: &str = "frontends";
/// Filename of the daemon's fixed namespace attach socket for local frontend
/// runners (`<config_dir>/frontends/local.sock`). One socket, always at this
/// name; auth is filesystem permissions on the socket.
pub(crate) const LOCAL_ATTACH_SOCKET_NAME: &str = "local.sock";
/// Filename of the UDS namespace attach listener under `frontends/`
/// (`<config_dir>/frontends/vsock-attach.sock`). Bound on demand via the
/// daemon's `AttachVsock` control operation, one per daemon instance. Libkrun's
/// vsock-proxy path terminates every guest vsock dial on this socket as the
/// same local peer; bind locality is the auth, same as TCP loopback/docker0.
pub(crate) const VSOCK_ATTACH_SOCKET_NAME: &str = "vsock-attach.sock";
/// Durable TCP and vsock listener authority.
pub(crate) const ATTACH_TARGETS_FILE: &str = "targets.json";
pub const OMNIFS_HOME_ENV: &str = "OMNIFS_HOME";
/// Overrides the host-visible mount point the daemon serves at.
pub const OMNIFS_MOUNT_POINT_ENV: &str = "OMNIFS_MOUNT_POINT";

/// Path resolution failed because no default root could be derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveError;

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cannot resolve omnifs home: set HOME or OMNIFS_HOME")
    }
}

impl std::error::Error for ResolveError {}

/// Resolve the omnifs home root from `OMNIFS_HOME`, else `$HOME/.omnifs`.
fn resolve_root() -> Result<PathBuf, ResolveError> {
    let omnifs_home = std::env::var_os(OMNIFS_HOME_ENV).map(PathBuf::from);
    let default_root =
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(DEFAULT_HOME_SUBDIR));
    omnifs_home.or(default_root).ok_or(ResolveError)
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
/// Callers derive it from a cache dir through here so the relative name cannot
/// drift between warmup storage and the host runtime.
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

pub struct Workspace {
    config_file: PathBuf,
    credentials: FileStore,
    catalog: Catalog,
    desired_state: DesiredState,
    daemon: DaemonState,
    frontend: FrontendState,
    identity: WorkspaceIdentity,
    metrics: crate::metrics::Store,
    warmup: WarmupStore,
}

impl Workspace {
    /// Resolve the workspace from `OMNIFS_HOME` or `$HOME/.omnifs`.
    pub fn resolve() -> Result<Self, ResolveError> {
        Ok(Self::under_root(&resolve_root()?))
    }

    /// Construct a workspace under a fixture or explicitly selected root.
    /// Relative roots are normalized once at this boundary.
    #[must_use]
    pub fn under_root(root: &Path) -> Self {
        let home = absolute(root);
        let cache_dir = home.join(CACHE_SUBDIR);
        let credentials = FileStore::new(home.join(CREDENTIALS_FILE));
        let catalog = Catalog::open(home.join(PROVIDERS_SUBDIR));
        let daemon = DaemonState::new(home.clone());
        let frontend = FrontendState::new(home.clone(), cache_dir.clone());
        let identity = WorkspaceIdentity { home: home.clone() };
        Self {
            config_file: home.join(CONFIG_FILE),
            credentials,
            catalog,
            desired_state: DesiredState::new(home.join(MOUNTS_SUBDIR), cache_dir.clone()),
            daemon,
            frontend,
            identity,
            metrics: crate::metrics::Store::new(&home),
            warmup: WarmupStore::new(home, cache_dir),
        }
    }

    /// Load the workspace configuration lazily from its owned file.
    pub fn config(&self) -> Result<Config, crate::config::ConfigError> {
        Config::load(&self.config_file)
    }

    /// Diagnostic information for the final CLI presentation boundary.
    #[must_use]
    pub fn config_diagnostic(&self) -> ConfigDiagnostic {
        ConfigDiagnostic {
            exists: self.config_file.exists(),
            display: display(&self.config_file),
        }
    }

    #[must_use]
    pub fn credentials(&self) -> &FileStore {
        &self.credentials
    }

    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    #[must_use]
    pub fn desired_state(&self) -> &DesiredState {
        &self.desired_state
    }

    #[must_use]
    pub fn daemon(&self) -> &DaemonState {
        &self.daemon
    }

    #[must_use]
    pub fn frontend(&self) -> &FrontendState {
        &self.frontend
    }

    /// Workspace identity consumed by Docker naming and final output.
    #[must_use]
    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }

    #[must_use]
    pub fn metrics(&self) -> &crate::metrics::Store {
        &self.metrics
    }

    #[must_use]
    pub fn warmup(&self) -> &WarmupStore {
        &self.warmup
    }
}

/// Workspace identity consumed only by Docker naming and final output.
#[derive(Debug)]
pub struct WorkspaceIdentity {
    home: PathBuf,
}

impl WorkspaceIdentity {
    #[must_use]
    pub fn container_label(&self) -> &Path {
        &self.home
    }

    #[must_use]
    pub fn output_home(&self) -> PathBuf {
        self.home.clone()
    }
}

#[derive(Debug)]
pub struct ConfigDiagnostic {
    pub exists: bool,
    pub display: String,
}

// This is intentionally a typed owner rather than a public root/layout bag.
#[derive(Debug)]
pub struct DesiredState {
    mounts_dir: PathBuf,
    cache_dir: PathBuf,
    repository: OnceCell<RefCell<Repository>>,
}

impl DesiredState {
    fn new(mounts_dir: PathBuf, cache_dir: PathBuf) -> Self {
        Self {
            mounts_dir,
            cache_dir,
            repository: OnceCell::new(),
        }
    }

    pub fn registry(&self) -> Result<Registry, SpecError> {
        std::fs::create_dir_all(&self.mounts_dir).map_err(|source| SpecError::ScanMounts {
            path: self.mounts_dir.clone(),
            source,
        })?;
        Registry::load(&self.mounts_dir)
    }

    pub fn repository(&self) -> Result<RefMut<'_, Repository>, crate::mounts::RepositoryError> {
        if let Some(cell) = self.repository.get() {
            return Ok(cell.borrow_mut());
        }
        let repository = Repository::open(&self.mounts_dir)?;
        Ok(self
            .repository
            .get_or_init(|| RefCell::new(repository))
            .borrow_mut())
    }

    pub fn observe_repository(&self) -> Result<Repository, crate::mounts::RepositoryError> {
        Repository::observe(&self.mounts_dir)
    }

    pub fn put_uncommitted(&self, spec: &Spec) -> Result<(), crate::mounts::RepositoryError> {
        self.repository()?.put(spec)
    }

    pub fn remove_uncommitted(&self, name: &Name) -> Result<bool, crate::mounts::RepositoryError> {
        self.repository()?.remove(name)
    }

    pub fn commit(&self) -> Result<Revision, crate::mounts::RepositoryError> {
        self.repository()?.commit()
    }

    #[must_use]
    pub fn repository_exists(&self) -> bool {
        self.mounts_dir.is_dir()
    }

    #[must_use]
    pub fn repository_display(&self) -> String {
        self.mounts_dir.display().to_string()
    }

    pub fn snapshot(
        &self,
        repository: &Repository,
        revision: &Revision,
    ) -> Result<(PathBuf, Registry), crate::mounts::RepositoryError> {
        repository.snapshot(revision, &self.cache_dir)
    }

    /// Exact spec path at a filesystem or final output boundary.
    #[must_use]
    pub fn spec_path(&self, name: &Name) -> PathBuf {
        self.mounts_dir.join(format!("{name}.json"))
    }
}

/// Persistent daemon control and runtime state for one workspace.
///
/// This component owns daemon-record I/O and derives concrete paths only for
/// the immediate socket, process, runtime, or diagnostic boundary that needs
/// them. It is cloneable because the daemon keeps it across spawned tasks.
#[derive(Debug, Clone)]
pub struct DaemonState {
    home: PathBuf,
}

impl DaemonState {
    fn new(home: PathBuf) -> Self {
        Self { home }
    }

    pub fn record(&self) -> io::Result<Option<crate::daemon_record::DaemonRecord>> {
        crate::daemon_record::DaemonRecord::read(&self.record_file())
    }

    pub fn write_record(&self, record: &crate::daemon_record::DaemonRecord) -> io::Result<()> {
        record.write(&self.record_file())
    }

    pub fn remove_record(&self) -> io::Result<()> {
        crate::daemon_record::DaemonRecord::remove(&self.record_file())
    }

    #[must_use]
    pub fn record_file(&self) -> PathBuf {
        self.home.join(crate::daemon_record::DAEMON_RECORD_FILE)
    }

    #[must_use]
    pub fn control_socket(&self) -> PathBuf {
        self.home.join(crate::daemon_record::CONTROL_SOCKET_FILE)
    }

    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.cache_dir().join("daemon.log")
    }

    #[must_use]
    pub fn clone_cache(&self) -> PathBuf {
        self.cache_dir().join("clones")
    }

    #[must_use]
    pub fn mount_snapshot(&self, revision: &Revision) -> PathBuf {
        Repository::snapshot_path(&self.cache_dir(), revision)
    }

    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.home
    }

    #[must_use]
    pub fn cache_dir(&self) -> PathBuf {
        self.home.join(CACHE_SUBDIR)
    }

    #[must_use]
    pub fn providers_dir(&self) -> PathBuf {
        self.home.join(PROVIDERS_SUBDIR)
    }

    #[must_use]
    pub fn credentials_file(&self) -> PathBuf {
        self.home.join(CREDENTIALS_FILE)
    }
}

/// Frontend lifecycle state and launch resources for one workspace.
///
/// The component owns durable attach authority, runner discovery leaves, and
/// frontend-specific runtime locations. It derives a concrete path only when a
/// frontend process, state scan, teardown, or output boundary consumes it.
#[derive(Debug, Clone)]
pub struct FrontendState {
    config_dir: PathBuf,
    cache_dir: PathBuf,
    state_root: PathBuf,
}

impl FrontendState {
    fn new(config_dir: PathBuf, cache_dir: PathBuf) -> Self {
        let state_root = cache_dir.join(FRONTEND_STATE_SUBDIR);
        Self {
            config_dir,
            cache_dir,
            state_root,
        }
    }

    pub fn attach_store(&self) -> io::Result<crate::attach::Store> {
        crate::attach::Store::open(
            self.config_dir
                .join(FRONTENDS_SUBDIR)
                .join(ATTACH_TARGETS_FILE),
        )
    }

    #[must_use]
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }
    #[must_use]
    pub fn state_dir(&self, kind: crate::daemon_record::FrontendKind, mount: &Path) -> PathBuf {
        let normalized = mount.components().collect::<PathBuf>();
        let digest = blake3::hash(normalized.as_os_str().as_encoded_bytes()).to_hex();
        self.state_root.join(kind.label()).join(digest.as_str())
    }

    #[must_use]
    pub fn local_attach_socket(&self) -> PathBuf {
        self.config_dir
            .join(FRONTENDS_SUBDIR)
            .join(LOCAL_ATTACH_SOCKET_NAME)
    }

    #[must_use]
    pub fn vsock_attach_socket(&self) -> PathBuf {
        self.config_dir
            .join(FRONTENDS_SUBDIR)
            .join(VSOCK_ATTACH_SOCKET_NAME)
    }

    #[must_use]
    pub fn host_log(&self, kind: crate::daemon_record::FrontendKind) -> PathBuf {
        self.cache_dir
            .join(format!("frontend-{}.log", kind.label()))
    }

    #[must_use]
    pub fn libkrun_root(&self) -> PathBuf {
        self.config_dir.join("libkrun")
    }

    #[must_use]
    pub fn guest_image_cache(&self) -> PathBuf {
        self.cache_dir.join("guest-images")
    }

    #[must_use]
    pub fn default_host_location(&self) -> PathBuf {
        self.config_dir.join("omnifs")
    }
}

/// Persistent provider warmup lock/progress storage.
#[derive(Debug, Clone)]
pub struct WarmupStore {
    config_dir: PathBuf,
    cache_dir: PathBuf,
}

/// Durable progress record for one detached provider warmup process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarmupProgress {
    pub pid: u32,
    pub completed: usize,
    pub total: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WarmupStore {
    fn new(config_dir: PathBuf, cache_dir: PathBuf) -> Self {
        Self {
            config_dir,
            cache_dir,
        }
    }
    #[must_use]
    pub fn workspace_home(&self) -> &Path {
        &self.config_dir
    }
    pub fn prepare(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.cache_dir)
    }
    #[must_use]
    fn lock_file(&self) -> PathBuf {
        self.cache_dir.join("provider-warmup.lock")
    }
    #[must_use]
    fn progress_file(&self) -> PathBuf {
        self.cache_dir.join("provider-warmup.json")
    }
    #[must_use]
    pub fn wasm_cache_dir(&self) -> PathBuf {
        wasm_cache_dir(&self.cache_dir)
    }

    pub fn acquire(&self) -> io::Result<File> {
        std::fs::create_dir_all(&self.cache_dir)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(self.lock_file())?;
        file.lock_exclusive()?;
        Ok(file)
    }

    pub fn is_active(&self) -> io::Result<bool> {
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.lock_file())
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match file.try_lock_exclusive() {
            Ok(()) => {
                file.unlock()?;
                Ok(false)
            },
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error),
        }
    }

    pub fn read_progress(&self) -> io::Result<Option<WarmupProgress>> {
        let bytes = match std::fs::read(self.progress_file()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub fn write_progress(&self, progress: &WarmupProgress) -> io::Result<()> {
        std::fs::create_dir_all(&self.cache_dir)?;
        let bytes = serde_json::to_vec(progress).map_err(io::Error::other)?;
        let mut options = AtomicOpenOptions::new();
        #[cfg(unix)]
        {
            use atomic_write_file::unix::OpenOptionsExt as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            options.preserve_mode(false).mode(0o600);
        }
        let mut file = options.open(self.progress_file())?;
        file.write_all(&bytes)?;
        file.commit()
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ProviderId;

    fn spec(name: &str) -> Spec {
        serde_json::from_value(serde_json::json!({
            "provider": {
                "id": ProviderId::from_wasm_bytes(name.as_bytes()).to_string(),
                "meta": { "name": name }
            },
            "mount": name
        }))
        .unwrap()
    }

    #[test]
    fn relative_roots_are_absolute_at_the_workspace_boundary() {
        let workspace = Workspace::under_root(Path::new(".omnifs-test"));
        assert!(workspace.identity().output_home().is_absolute());
        assert!(workspace.daemon().cache_dir().is_absolute());
    }

    #[test]
    fn daemon_state_resolves_the_requested_mount_snapshot() {
        let workspace = Workspace::under_root(Path::new("/tmp/omnifs-test"));
        let first = Revision::new("a".repeat(40)).unwrap();
        let second = Revision::new("b".repeat(40)).unwrap();
        let daemon = workspace.daemon();

        assert_ne!(
            daemon.mount_snapshot(&first),
            daemon.mount_snapshot(&second)
        );
        assert!(
            daemon
                .mount_snapshot(&second)
                .ends_with(Path::new("mount-revisions").join(second.as_str()))
        );
    }

    #[test]
    fn desired_state_reads_after_write_and_remove() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::under_root(temp.path());
        assert!(
            workspace
                .desired_state()
                .registry()
                .unwrap()
                .iter()
                .next()
                .is_none()
        );

        workspace
            .desired_state()
            .put_uncommitted(&spec("github"))
            .unwrap();
        let registry = workspace.desired_state().registry().unwrap();
        assert_eq!(registry.iter().count(), 1);
        drop(registry);

        workspace
            .desired_state()
            .remove_uncommitted(&Name::new("github").unwrap())
            .unwrap();
        assert!(
            workspace
                .desired_state()
                .registry()
                .unwrap()
                .iter()
                .next()
                .is_none()
        );
    }
}
