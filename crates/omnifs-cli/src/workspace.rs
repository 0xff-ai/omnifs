//! Command-facing view of the resolved omnifs home.
//!
//! `Workspace` owns the local layout for one CLI invocation and is the factory
//! for command-scoped handles derived from that layout: config, provider
//! catalog, daemon client, and configured mounts.

use omnifs_workspace::layout::{Workspace as HomeWorkspace, WorkspaceLayout};
use omnifs_workspace::mounts::{Name as MountName, Registry, SpecError};
use omnifs_workspace::provider::Catalog;
use std::cell::{OnceCell, Ref, RefCell};
use std::path::PathBuf;

use crate::client::DaemonClient;
use crate::config::Config;
use crate::credential_target::CredentialTarget;
use crate::mount_config::MountConfig;
use omnifs_workspace::mounts::Spec;

/// Resolved local omnifs home for one CLI command.
pub(crate) struct Workspace {
    home: HomeWorkspace,
    catalog: Catalog,
    daemon: DaemonClient,
    /// The mount-spec registry, loaded once per command and reused. Disk is
    /// still the source of truth across processes; within one process this
    /// is the single owner of mount-spec mutation too (`put_mount`,
    /// `remove_mount`), not just a read mirror. A command that writes a spec
    /// and later reads mounts again (`omnifs setup` configures providers,
    /// then launches the daemon) must see its own write, so every write path
    /// goes through this same cell instead of a throwaway `Registry::load`.
    registry: OnceCell<RefCell<Registry>>,
}

#[derive(Debug, Clone)]
pub(crate) struct MountRemovalTarget {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) config: Option<Spec>,
    pub(crate) credential: CredentialTarget,
}

impl Workspace {
    pub(crate) fn resolve() -> anyhow::Result<Self> {
        let layout = WorkspaceLayout::resolve()?;
        Ok(Self::from_layout(layout))
    }

    pub(crate) fn from_layout(layout: WorkspaceLayout) -> Self {
        Self::from_home(HomeWorkspace::from_layout(layout))
    }

    pub(crate) fn from_home(home: HomeWorkspace) -> Self {
        let catalog = Catalog::open(home.providers_dir());
        let daemon = DaemonClient::for_layout(home.layout());
        Self {
            home,
            catalog,
            daemon,
            registry: OnceCell::new(),
        }
    }

    /// The mount-spec registry cell for this command, scanned from `mounts/`
    /// on first use and cached for the lifetime of this `Workspace`. Every
    /// read (`mounts()`, `reset_removal_targets()`) and every write
    /// (`put_mount`, `remove_mount`) goes through this one cell, so a write
    /// earlier in a multi-step command is visible to a later read in the
    /// same process instead of a stale pre-write snapshot.
    fn registry_cell(&self) -> Result<&RefCell<Registry>, SpecError> {
        if let Some(cell) = self.registry.get() {
            return Ok(cell);
        }
        let registry = Registry::load(self.home.mounts_dir())?;
        Ok(self.registry.get_or_init(|| RefCell::new(registry)))
    }

    fn registry(&self) -> anyhow::Result<Ref<'_, Registry>> {
        Ok(self.registry_cell()?.borrow())
    }

    /// Persist `spec` through this command's shared registry. Later
    /// `mounts()` / `reset_removal_targets()` calls in this process observe
    /// the write immediately.
    pub(crate) fn put_mount(&self, spec: &Spec) -> Result<(), SpecError> {
        self.registry_cell()?.borrow_mut().put(spec)
    }

    /// Remove a mount's spec through this command's shared registry, so a
    /// later `mounts()` read in the same process no longer sees it. Mirrors
    /// [`Registry::remove`]: `Ok(false)` when no file was present.
    pub(crate) fn remove_mount(&self, name: &MountName) -> Result<bool, SpecError> {
        self.registry_cell()?.borrow_mut().remove(name)
    }

    pub(crate) fn layout(&self) -> &WorkspaceLayout {
        self.home.layout()
    }

    pub(crate) fn config(&self) -> anyhow::Result<Config> {
        Config::load(self.home.config_file())
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

    /// Build removal targets tolerantly, for use by `omnifs reset`.
    ///
    /// Reads through the shared [`Registry`]: resolvable specs yield a target
    /// plus their stored credential; files that failed to load (broken JSON,
    /// name/filename mismatch) still produce a target with
    /// `CredentialTarget::None` so reset can nuke broken state.
    pub(crate) fn reset_removal_targets(&self) -> anyhow::Result<Vec<MountRemovalTarget>> {
        let registry = self.registry()?;
        let mut targets = Vec::new();

        for (name, spec) in registry.iter() {
            targets.push(MountRemovalTarget {
                name: name.to_string(),
                path: registry.spec_path(name),
                config: Some(spec.clone()),
                credential: CredentialTarget::for_mount(spec),
            });
        }

        for failure in registry.failures() {
            let Some(name) = failure
                .path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            tracing::warn!(
                path = %failure.path.display(),
                error = %failure.error,
                "unparsable mount config; will remove the file but cannot drop credentials"
            );
            targets.push(MountRemovalTarget {
                name,
                path: failure.path.clone(),
                config: None,
                credential: CredentialTarget::None,
            });
        }

        targets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(targets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{fixture_paths, spec_with_provider};

    /// Reproduces the `omnifs setup` bug: `configure_and_launch` calls
    /// `workspace.mounts()` before any provider is configured (an empty
    /// read), then `run_init_loop` writes new specs, then `Launcher::launch`
    /// calls `workspace.mounts()` again on the *same* `Workspace` and must
    /// see the write, not the pre-write empty snapshot the cache captured.
    #[test]
    fn mounts_observes_a_put_after_an_earlier_empty_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = fixture_paths(tmp.path());
        std::fs::create_dir_all(&paths.mounts_dir).unwrap();
        let workspace = Workspace::from_layout(paths);

        // Warm the cache empty, mirroring the early `workspace.mounts()?`
        // call in `configure_and_launch` before any provider is configured.
        assert!(workspace.mounts().unwrap().is_empty());

        // Write a spec the way `persist_mount_spec` does when the daemon is
        // not yet running (the common case during first-run `omnifs setup`).
        let spec = spec_with_provider("github", r#"{ "mount": "github" }"#);
        workspace.put_mount(&spec).unwrap();

        // The launch preflight's `self.workspace.mounts()?` must observe it.
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
        workspace.put_mount(&spec).unwrap();
        assert_eq!(workspace.mounts().unwrap().len(), 1);

        let name = omnifs_workspace::mounts::Name::new("github".to_owned()).unwrap();
        assert!(workspace.remove_mount(&name).unwrap());

        assert!(
            workspace.mounts().unwrap().is_empty(),
            "a mount removed after an earlier read must be gone from a later read"
        );
    }
}
