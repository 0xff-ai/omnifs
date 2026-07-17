//! The workspace broker for persistent omnifs state.

use std::cell::{OnceCell, RefCell, RefMut};
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::creds::FileStore;
use crate::layout::{self, WorkspaceLayout};
use crate::mounts::{Name, Registry, Repository, Revision, Spec, SpecError};
use crate::provider::Catalog;
use atomic_write_file::OpenOptions as AtomicOpenOptions;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

/// The central broker for one omnifs home.
///
/// `Workspace` owns the persistent components under `OMNIFS_HOME`, not a
/// bag of paths. It never exposes [`WorkspaceLayout`], the home root, or
/// generic directory getters. Callers request a typed component owner and a
/// concrete path can leave that owner only at the immediate filesystem,
/// process, protocol, engine, test-fixture, or final-output boundary that
/// consumes it.
pub struct Workspace {
    config_file: PathBuf,
    credentials: FileStore,
    catalog: Catalog,
    desired_state: DesiredState,
    daemon: DaemonFiles,
    frontend: FrontendFiles,
    metrics: crate::metrics::Store,
    warmup: WarmupStore,
}

impl Workspace {
    /// Resolve the workspace from `OMNIFS_HOME` or `$HOME/.omnifs`.
    pub fn resolve() -> Result<Self, layout::ResolveError> {
        let layout = WorkspaceLayout::resolve()?;
        Ok(Self::from_layout(layout))
    }

    /// Construct a workspace under a fixture or explicitly selected root.
    /// Relative roots are normalized once at this boundary.
    #[must_use]
    pub fn under_root(root: &Path) -> Self {
        Self::from_layout(WorkspaceLayout::under_root(&absolute(root)))
    }

    fn from_layout(layout: WorkspaceLayout) -> Self {
        let layout = absolute_layout(layout);
        let mounts_dir = layout.mounts_dir.clone();
        let cache_dir = layout.cache_dir.clone();
        let config_dir = layout.config_dir.clone();
        Self {
            config_file: layout.config_file.clone(),
            credentials: FileStore::new(layout.credentials_file.clone()),
            catalog: Catalog::open(layout.providers_dir.clone()),
            desired_state: DesiredState::new(mounts_dir, cache_dir.clone()),
            daemon: DaemonFiles::new(layout),
            frontend: FrontendFiles::new(config_dir.clone(), cache_dir.clone()),
            metrics: crate::metrics::Store::new(&config_dir),
            warmup: WarmupStore::new(config_dir, cache_dir),
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
            display: layout::display(&self.config_file),
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
    pub fn daemon(&self) -> &DaemonFiles {
        &self.daemon
    }

    #[must_use]
    pub fn frontend(&self) -> &FrontendFiles {
        &self.frontend
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

#[derive(Debug, Clone)]
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

/// Daemon and control-plane persistent resources.
#[derive(Debug, Clone)]
pub struct DaemonFiles {
    config_dir: PathBuf,
    cache_dir: PathBuf,
    providers_dir: PathBuf,
    credentials_file: PathBuf,
    record_file: PathBuf,
    control_socket: PathBuf,
    attach_targets_file: PathBuf,
    local_attach_socket: PathBuf,
    vsock_attach_socket: PathBuf,
    mount_revisions_root: PathBuf,
}

impl DaemonFiles {
    fn new(layout: WorkspaceLayout) -> Self {
        let record_file = layout.daemon_record_file();
        let control_socket = layout.control_socket();
        let attach_targets_file = layout.attach_targets_file();
        let local_attach_socket = layout.local_attach_socket();
        let vsock_attach_socket = layout.vsock_attach_socket();
        let mount_revisions_root = layout.mount_revisions_root();
        Self {
            config_dir: layout.config_dir,
            cache_dir: layout.cache_dir,
            providers_dir: layout.providers_dir,
            credentials_file: layout.credentials_file,
            record_file,
            control_socket,
            attach_targets_file,
            local_attach_socket,
            vsock_attach_socket,
            mount_revisions_root,
        }
    }

    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
    #[must_use]
    pub fn clone_cache(&self) -> PathBuf {
        self.cache_dir.join("clones")
    }
    #[must_use]
    pub fn providers_dir(&self) -> &Path {
        &self.providers_dir
    }
    #[must_use]
    pub fn credentials_file(&self) -> &Path {
        &self.credentials_file
    }
    #[must_use]
    pub fn record_file(&self) -> &Path {
        &self.record_file
    }
    #[must_use]
    pub fn control_socket(&self) -> &Path {
        &self.control_socket
    }
    pub fn attach_store(&self) -> io::Result<crate::attach::Store> {
        crate::attach::Store::open(self.attach_targets_file.clone())
    }
    #[must_use]
    pub fn local_attach_socket(&self) -> &Path {
        &self.local_attach_socket
    }
    #[must_use]
    pub fn vsock_attach_socket(&self) -> &Path {
        &self.vsock_attach_socket
    }
    #[must_use]
    pub fn mount_snapshot(&self, revision: &Revision) -> PathBuf {
        self.mount_revisions_root.join(revision.as_str())
    }
    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.cache_dir.join("daemon.log")
    }
}

/// Persistent frontend state and process-boundary paths.
#[derive(Debug, Clone)]
pub struct FrontendFiles {
    config_dir: PathBuf,
    cache_dir: PathBuf,
}

impl FrontendFiles {
    fn new(config_dir: PathBuf, cache_dir: PathBuf) -> Self {
        Self {
            config_dir,
            cache_dir,
        }
    }
    #[must_use]
    pub fn workspace_label(&self) -> &Path {
        &self.config_dir
    }
    #[must_use]
    pub fn home_for_output(&self) -> PathBuf {
        self.config_dir.clone()
    }
    #[must_use]
    pub fn default_host_location(&self) -> PathBuf {
        self.config_dir.join("omnifs")
    }
    #[must_use]
    pub fn docker_home(&self) -> &Path {
        &self.config_dir
    }
    #[must_use]
    pub fn guest_image_cache(&self) -> PathBuf {
        self.cache_dir.join("guest-images")
    }
    #[must_use]
    pub fn libkrun_root(&self) -> PathBuf {
        self.config_dir.join("libkrun")
    }
    #[must_use]
    pub fn host_log(&self, protocol: &str) -> PathBuf {
        self.cache_dir.join(format!("frontend-{protocol}.log"))
    }
    #[must_use]
    pub fn frontend_state_root(&self) -> PathBuf {
        self.cache_dir.join(layout::FRONTEND_STATE_SUBDIR)
    }
    #[must_use]
    pub fn state_dir(&self, kind: crate::daemon_record::FrontendKind, mount: &Path) -> PathBuf {
        let normalized = mount.components().collect::<PathBuf>();
        let digest = blake3::hash(normalized.as_os_str().as_encoded_bytes()).to_hex();
        self.frontend_state_root()
            .join(kind.label())
            .join(digest.as_str())
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
        layout::wasm_cache_dir(&self.cache_dir)
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

fn absolute_layout(mut layout: WorkspaceLayout) -> WorkspaceLayout {
    layout.config_dir = absolute(&layout.config_dir);
    layout.cache_dir = absolute(&layout.cache_dir);
    layout.mounts_dir = absolute(&layout.mounts_dir);
    layout.providers_dir = absolute(&layout.providers_dir);
    layout.credentials_file = absolute(&layout.credentials_file);
    layout.config_file = absolute(&layout.config_file);
    layout
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
        assert!(workspace.frontend().workspace_label().is_absolute());
        assert!(workspace.daemon().log_file().is_absolute());
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
