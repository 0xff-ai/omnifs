//! Command-facing view of the resolved omnifs home.
//!
//! `Workspace` owns the local layout for one CLI invocation and is the factory
//! for command-scoped handles derived from that layout: config, provider
//! catalog, daemon client, and configured mounts.

use omnifs_workspace::creds::FileStore;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::mounts::{Name as MountName, Registry, Repository, Revision, SpecError};
use omnifs_workspace::provider::Catalog;
use std::cell::{OnceCell, Ref, RefCell, RefMut};
use std::path::{Path, PathBuf};

use crate::client::DaemonClient;
use crate::docker::ContainerName;
use crate::frontend_container::frontend_container_name;
use crate::host_runner::{HostRunner, LocalProtocol};
use crate::libkrun_runner::LibkrunRunner;
use crate::mount_config::MountConfig;
use crate::provider_warmup::ProviderWarmup;
use omnifs_workspace::config::Config;
use omnifs_workspace::mounts::Spec;

/// Command-scoped composition root for one resolved omnifs home.
///
/// `Workspace` never exposes [`WorkspaceLayout`] or raw paths into
/// `OMNIFS_HOME`. It returns typed owners or domain handles; concrete paths
/// leave those owners only at filesystem, process, protocol, or final output
/// boundaries.
pub(crate) struct Workspace {
    desired_state: DesiredState,
    frontend: FrontendOwner,
    config_file: PathBuf,
    metrics: MetricsOwner,
    provider_warmup: ProviderWarmup,
    catalog: Catalog,
    daemon: DaemonClient,
    credentials: FileStore,
}

impl Workspace {
    pub(crate) fn resolve() -> anyhow::Result<Self> {
        let layout = WorkspaceLayout::resolve()?;
        Ok(Self::from_layout(layout))
    }

    pub(crate) fn from_layout(layout: WorkspaceLayout) -> Self {
        let layout = absolute_layout(layout);
        let desired_state = DesiredState::new(layout.mounts_dir.clone(), layout.cache_dir.clone());
        let frontend = FrontendOwner::new(layout.config_dir.clone(), layout.cache_dir.clone());
        let catalog = Catalog::open(&layout.providers_dir);
        let daemon = DaemonClient::for_layout(&layout);
        let credentials = FileStore::new(layout.credentials_file.clone());
        let provider_warmup = ProviderWarmup::new(&layout);
        Self {
            desired_state,
            frontend,
            config_file: layout.config_file,
            metrics: MetricsOwner::new(layout.config_dir.clone()),
            provider_warmup,
            catalog,
            daemon,
            credentials,
        }
    }

    pub(crate) fn config(&self) -> anyhow::Result<Config> {
        Ok(Config::load(&self.config_file)?)
    }

    pub(crate) fn config_diagnostic(&self) -> ConfigDiagnostic {
        ConfigDiagnostic {
            exists: self.config_file.exists(),
            display: omnifs_workspace::layout::display(&self.config_file),
        }
    }

    pub(crate) fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub(crate) fn daemon(&self) -> &DaemonClient {
        &self.daemon
    }

    pub(crate) fn credentials(&self) -> &FileStore {
        &self.credentials
    }

    pub(crate) fn frontend(&self) -> &FrontendOwner {
        &self.frontend
    }

    pub(crate) fn desired_state(&self) -> &DesiredState {
        &self.desired_state
    }

    pub(crate) fn metrics(&self) -> &MetricsOwner {
        &self.metrics
    }

    pub(crate) fn provider_warmup(&self) -> &ProviderWarmup {
        &self.provider_warmup
    }
}

pub(crate) struct DesiredState {
    mounts_dir: PathBuf,
    cache_dir: PathBuf,
    registry: OnceCell<RefCell<Registry>>,
    repository: OnceCell<RefCell<Repository>>,
}

pub(crate) struct ConfigDiagnostic {
    pub exists: bool,
    pub display: String,
}

pub(crate) struct MetricsOwner {
    config_dir: PathBuf,
}
impl MetricsOwner {
    fn new(config_dir: PathBuf) -> Self {
        Self { config_dir }
    }
    pub(crate) fn last_nudge(&self) -> PathBuf {
        self.config_dir
            .join(omnifs_workspace::metrics::SUBDIR)
            .join("last-nudge")
    }
    pub(crate) fn sink(&self, enabled: bool) -> omnifs_workspace::metrics::Sink {
        omnifs_workspace::metrics::Sink::new(&self.config_dir, enabled)
    }
}

impl DesiredState {
    fn new(mounts_dir: PathBuf, cache_dir: PathBuf) -> Self {
        Self {
            mounts_dir,
            cache_dir,
            registry: OnceCell::new(),
            repository: OnceCell::new(),
        }
    }

    fn registry_cell(&self) -> Result<&RefCell<Registry>, SpecError> {
        if let Some(cell) = self.registry.get() {
            return Ok(cell);
        }
        std::fs::create_dir_all(&self.mounts_dir).map_err(|source| SpecError::ScanMounts {
            path: self.mounts_dir.clone(),
            source,
        })?;
        let registry = Registry::load(&self.mounts_dir)?;
        Ok(self.registry.get_or_init(|| RefCell::new(registry)))
    }

    fn registry(&self) -> anyhow::Result<Ref<'_, Registry>> {
        Ok(self.registry_cell()?.borrow())
    }

    fn refresh_registry(&self) -> anyhow::Result<()> {
        if let Some(cell) = self.registry.get() {
            cell.borrow_mut().reload()?;
        }
        Ok(())
    }

    pub(crate) fn repository(&self) -> anyhow::Result<RefMut<'_, Repository>> {
        if let Some(cell) = self.repository.get() {
            return Ok(cell.borrow_mut());
        }
        let repository = Repository::open(&self.mounts_dir)?;
        Ok(self
            .repository
            .get_or_init(|| RefCell::new(repository))
            .borrow_mut())
    }

    pub(crate) fn observe_repository(&self) -> anyhow::Result<Repository> {
        Ok(Repository::observe(&self.mounts_dir)?)
    }

    pub(crate) fn put_uncommitted(&self, spec: &Spec) -> anyhow::Result<()> {
        {
            let mut repository = self.repository()?;
            repository.put(spec)?;
        }
        self.refresh_registry()
    }

    pub(crate) fn remove_uncommitted(&self, name: &MountName) -> anyhow::Result<bool> {
        let removed = {
            let mut repository = self.repository()?;
            repository.remove(name)?
        };
        self.refresh_registry()?;
        Ok(removed)
    }

    pub(crate) fn commit(&self) -> anyhow::Result<Revision> {
        let revision = {
            let mut repository = self.repository()?;
            repository.commit()?
        };
        self.refresh_registry()?;
        Ok(revision)
    }

    pub(crate) fn repository_exists(&self) -> bool {
        self.mounts_dir.is_dir()
    }

    pub(crate) fn repository_display(&self) -> String {
        self.mounts_dir.display().to_string()
    }

    pub(crate) fn snapshot(
        &self,
        repository: &Repository,
        revision: &Revision,
    ) -> anyhow::Result<(PathBuf, Registry)> {
        Ok(repository.snapshot(revision, &self.cache_dir)?)
    }

    /// Exact spec path for a filesystem or final user-visible output boundary.
    pub(crate) fn spec_path(&self, name: &MountName) -> PathBuf {
        self.mounts_dir.join(format!("{name}.json"))
    }

    /// Enumerate the one cached Registry, failing on malformed specs.
    pub(crate) fn mounts(&self) -> anyhow::Result<Vec<MountConfig>> {
        let registry = self.registry()?;
        if let Some(failure) = registry.failures().first() {
            return Err(anyhow::anyhow!("{}", failure.error));
        }
        Ok(registry
            .iter()
            .map(|(name, spec)| MountConfig {
                name: name.clone(),
                config: spec.clone(),
                source: registry.spec_path(name),
            })
            .collect())
    }
}

#[derive(Clone)]
pub(crate) struct FrontendOwner {
    config_dir: PathBuf,
    cache_dir: PathBuf,
}

impl FrontendOwner {
    fn new(config_dir: PathBuf, cache_dir: PathBuf) -> Self {
        Self {
            config_dir,
            cache_dir,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(config_dir: PathBuf, cache_dir: PathBuf) -> Self {
        Self::new(config_dir, cache_dir)
    }

    pub(crate) fn default_host_location(&self) -> PathBuf {
        self.config_dir.join("omnifs")
    }

    /// Absolute workspace identity at the final inventory serialization boundary.
    pub(crate) fn home_for_output(&self) -> PathBuf {
        self.config_dir.clone()
    }

    pub(crate) fn container_name(&self) -> anyhow::Result<ContainerName> {
        frontend_container_name(&self.config_dir)
    }

    /// Workspace label supplied directly to the Docker process boundary.
    pub(crate) fn docker_home(&self) -> &Path {
        &self.config_dir
    }

    pub(crate) fn libkrun_runner(&self) -> LibkrunRunner {
        LibkrunRunner::new(self.config_dir.clone())
    }

    /// Cache root supplied directly to guest-image materialization.
    pub(crate) fn guest_image_cache(&self) -> &Path {
        &self.cache_dir
    }

    pub(crate) fn host_runner(
        &self,
        mount_point: PathBuf,
        protocol: LocalProtocol,
    ) -> anyhow::Result<HostRunner> {
        HostRunner::new(self, mount_point, protocol)
    }

    pub(crate) fn host_log(&self, protocol: &str) -> PathBuf {
        self.cache_dir.join(format!("frontend-{protocol}.log"))
    }

    pub(crate) fn local_attach_socket(&self) -> PathBuf {
        self.config_dir
            .join(omnifs_workspace::layout::FRONTENDS_SUBDIR)
            .join(omnifs_workspace::layout::LOCAL_ATTACH_SOCKET_NAME)
    }
    pub(crate) fn frontend_state_root(&self) -> PathBuf {
        self.cache_dir
            .join(omnifs_workspace::layout::FRONTEND_STATE_SUBDIR)
    }
    pub(crate) fn state_dir(
        &self,
        kind: omnifs_workspace::daemon_record::FrontendKind,
        mount: &std::path::Path,
    ) -> PathBuf {
        let normalized = mount.components().collect::<PathBuf>();
        let digest = blake3::hash(normalized.as_os_str().as_encoded_bytes()).to_hex();
        self.frontend_state_root()
            .join(kind.label())
            .join(digest.as_str())
    }
}

/// Keep command-facing workspace paths absolute even when `OMNIFS_HOME` (or a
/// test fixture root) was supplied as a relative path. The workspace crate
/// owns the layout shape; this CLI boundary owns the serialized command view,
/// so normalize once before constructing any handles.
fn absolute_layout(mut layout: WorkspaceLayout) -> WorkspaceLayout {
    let Ok(current_dir) = std::env::current_dir() else {
        return layout;
    };
    let absolute = |path: PathBuf| {
        if path.is_absolute() {
            path
        } else {
            current_dir.join(path)
        }
    };
    layout.config_dir = absolute(layout.config_dir);
    layout.cache_dir = absolute(layout.cache_dir);
    layout.mounts_dir = absolute(layout.mounts_dir);
    layout.providers_dir = absolute(layout.providers_dir);
    layout.credentials_file = absolute(layout.credentials_file);
    layout.config_file = absolute(layout.config_file);
    layout
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fixture_paths, spec_with_provider};

    /// A command may inspect desired state before writing a mount, then read it
    /// again before committing or applying. The second read must see the write,
    /// not the empty snapshot cached by the first read.
    #[test]
    fn mounts_observes_a_put_after_an_earlier_empty_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = fixture_paths(tmp.path());
        assert!(!paths.mounts_dir.exists());
        let workspace = Workspace::from_layout(paths.clone());

        // Warm the cache with an empty desired-state read.
        assert!(workspace.desired_state().mounts().unwrap().is_empty());

        // Write a spec the way `persist_mount_spec` does when the daemon is
        // not yet running (the common case before the first `omnifs up`).
        let spec = spec_with_provider("github", r#"{ "mount": "github" }"#);
        workspace.desired_state().put_uncommitted(&spec).unwrap();
        assert!(paths.mounts_dir.join(".git").is_dir());

        // The next command-local read must observe the new spec.
        let mounts = workspace.desired_state().mounts().unwrap();
        assert_eq!(
            mounts.len(),
            1,
            "a mount written after an earlier empty read must be visible to a later read \
             on the same Workspace"
        );
        assert_eq!(mounts[0].name.to_string(), "github");
    }

    /// Symmetric case for removal: `remove_mount` must also be visible to a
    /// later `mounts()` read on the same `Workspace`.
    #[test]
    fn mounts_observes_a_remove_after_an_earlier_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = fixture_paths(tmp.path());
        std::fs::create_dir_all(&paths.mounts_dir).unwrap();
        let workspace = Workspace::from_layout(paths);

        let spec = spec_with_provider("github", r#"{ "mount": "github" }"#);
        workspace.desired_state().put_uncommitted(&spec).unwrap();
        assert_eq!(workspace.desired_state().mounts().unwrap().len(), 1);

        let name = omnifs_workspace::mounts::Name::new("github".to_owned()).unwrap();
        assert!(workspace.desired_state().remove_uncommitted(&name).unwrap());

        assert!(
            workspace.desired_state().mounts().unwrap().is_empty(),
            "a mount removed after an earlier read must be gone from a later read"
        );
    }

    #[test]
    fn relative_layout_paths_are_absolute_at_the_cli_boundary() {
        let root = PathBuf::from(".omnifs");
        let workspace = Workspace::from_layout(WorkspaceLayout::under_root(&root));
        assert!(workspace.frontend().home_for_output().is_absolute());
        assert!(workspace.daemon().log_file().is_absolute());
    }
}
