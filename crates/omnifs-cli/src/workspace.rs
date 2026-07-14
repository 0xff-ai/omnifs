//! Command-facing view of the resolved omnifs home.
//!
//! `Workspace` owns the local layout for one CLI invocation and is the factory
//! for command-scoped handles derived from that layout: config, provider
//! catalog, daemon client, and configured mounts.

use omnifs_workspace::layout::{Workspace as HomeWorkspace, WorkspaceLayout};
use omnifs_workspace::mounts::{Name as MountName, Registry, Repository, SpecError};
use omnifs_workspace::provider::Catalog;
use std::cell::{OnceCell, Ref, RefCell, RefMut};
use std::path::PathBuf;

use crate::client::DaemonClient;
use crate::mount_config::MountConfig;
use omnifs_workspace::config::Config;
use omnifs_workspace::mounts::Spec;

/// Resolved local omnifs home for one CLI command.
pub(crate) struct Workspace {
    home: HomeWorkspace,
    catalog: Catalog,
    daemon: DaemonClient,
    /// Read-only mount registry, refreshed after repository writes.
    registry: OnceCell<RefCell<Registry>>,
    /// Desired-state repository retaining its lock for this command lifetime.
    repository: OnceCell<RefCell<Repository>>,
}

impl Workspace {
    pub(crate) fn resolve() -> anyhow::Result<Self> {
        let layout = WorkspaceLayout::resolve()?;
        Ok(Self::from_layout(layout))
    }

    pub(crate) fn from_layout(layout: WorkspaceLayout) -> Self {
        Self::from_home(HomeWorkspace::from_layout(absolute_layout(layout)))
    }

    pub(crate) fn from_home(home: HomeWorkspace) -> Self {
        let catalog = Catalog::open(home.providers_dir());
        let daemon = DaemonClient::for_layout(home.layout());
        Self {
            home,
            catalog,
            daemon,
            registry: OnceCell::new(),
            repository: OnceCell::new(),
        }
    }

    /// The mount-spec registry cell for this command, scanned from `mounts/`
    /// on first use and cached for the lifetime of this `Workspace`. Every
    /// read (`mounts()`) and every write
    /// (`put_mount`, `remove_mount`) goes through this one cell, so a write
    /// earlier in a multi-step command is visible to a later read in the
    /// same process instead of a stale pre-write snapshot.
    fn registry_cell(&self) -> Result<&RefCell<Registry>, SpecError> {
        if let Some(cell) = self.registry.get() {
            return Ok(cell);
        }
        std::fs::create_dir_all(self.home.mounts_dir()).map_err(|source| {
            SpecError::ScanMounts {
                path: self.home.mounts_dir().to_path_buf(),
                source,
            }
        })?;
        let registry = Registry::load(self.home.mounts_dir())?;
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

    /// Open the desired-state repository and retain its lock.
    pub(crate) fn repository(&self) -> anyhow::Result<RefMut<'_, Repository>> {
        if let Some(cell) = self.repository.get() {
            return Ok(cell.borrow_mut());
        }
        let repository = Repository::open(self.home.mounts_dir())?;
        Ok(self
            .repository
            .get_or_init(|| RefCell::new(repository))
            .borrow_mut())
    }

    /// Observe desired state without opening the mutating repository owner.
    /// Inventory uses this path so a status read never initializes or commits
    /// a Git repository as a side effect.
    pub(crate) fn observe_repository(&self) -> anyhow::Result<Repository> {
        Ok(Repository::observe(self.home.mounts_dir())?)
    }

    pub(crate) fn put_mount_uncommitted(&self, spec: &Spec) -> anyhow::Result<()> {
        {
            let mut repository = self.repository()?;
            repository.put(spec)?;
        }
        self.refresh_registry()
    }

    pub(crate) fn remove_mount_uncommitted(&self, name: &MountName) -> anyhow::Result<bool> {
        let removed = {
            let mut repository = self.repository()?;
            repository.remove(name)?
        };
        self.refresh_registry()?;
        Ok(removed)
    }

    pub(crate) fn commit_mounts(&self) -> anyhow::Result<omnifs_workspace::mounts::Revision> {
        let revision = {
            let mut repository = self.repository()?;
            repository.commit()?
        };
        self.refresh_registry()?;
        Ok(revision)
    }

    pub(crate) fn layout(&self) -> &WorkspaceLayout {
        self.home.layout()
    }

    pub(crate) fn config(&self) -> anyhow::Result<Config> {
        Ok(Config::load(self.home.config_file())?)
    }

    pub(crate) fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub(crate) fn daemon(&self) -> &DaemonClient {
        &self.daemon
    }

    /// The single mount-enumeration funnel used by every command.
    ///
    /// Reads one `Spec` per JSON file in the `mounts/` directory through the
    /// shared [`Registry`] and returns the list sorted by mount name. Strict by
    /// design: a malformed spec aborts enumeration rather than being silently
    /// skipped, matching the former per-file loader.
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
        assert!(workspace.mounts().unwrap().is_empty());

        // Write a spec the way `persist_mount_spec` does when the daemon is
        // not yet running (the common case during first-run `omnifs setup`).
        let spec = spec_with_provider("github", r#"{ "mount": "github" }"#);
        workspace.put_mount_uncommitted(&spec).unwrap();
        assert!(paths.mounts_dir.join(".git").is_dir());

        // The next command-local read must observe the new spec.
        let mounts = workspace.mounts().unwrap();
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
        workspace.put_mount_uncommitted(&spec).unwrap();
        assert_eq!(workspace.mounts().unwrap().len(), 1);

        let name = omnifs_workspace::mounts::Name::new("github".to_owned()).unwrap();
        assert!(workspace.remove_mount_uncommitted(&name).unwrap());

        assert!(
            workspace.mounts().unwrap().is_empty(),
            "a mount removed after an earlier read must be gone from a later read"
        );
    }

    #[test]
    fn relative_layout_paths_are_absolute_at_the_cli_boundary() {
        let root = PathBuf::from(".omnifs");
        let workspace = Workspace::from_layout(WorkspaceLayout::under_root(&root));
        assert!(workspace.layout().config_dir.is_absolute());
        assert!(workspace.layout().config_file.is_absolute());
        assert!(workspace.layout().providers_dir.is_absolute());
    }
}
