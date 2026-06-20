//! Command-facing view of the resolved omnifs home.
//!
//! `Workspace` owns the local layout for one CLI invocation and is the factory
//! for command-scoped handles derived from that layout: config, provider
//! catalog, daemon client, and configured mounts.

use anyhow::Context as _;
use omnifs_home::{PathOverrides, Paths};
use std::path::{Path, PathBuf};

use crate::catalog::ProviderCatalog;
use crate::client::DaemonClient;
use crate::config::Config;
use crate::session::MountConfig;

/// Resolved local omnifs home for one CLI command.
pub(crate) struct Workspace {
    paths: Paths,
    catalog: ProviderCatalog,
    daemon: DaemonClient,
}

impl Workspace {
    pub(crate) fn resolve_default() -> anyhow::Result<Self> {
        Self::resolve(PathOverrides::default())
    }

    pub(crate) fn resolve(overrides: PathOverrides) -> anyhow::Result<Self> {
        let paths = Paths::resolve(overrides)?;
        Ok(Self::new(paths))
    }

    pub(crate) fn new(paths: Paths) -> Self {
        let catalog = ProviderCatalog::for_dirs(&paths.mounts_dir, &paths.providers_dir);
        let daemon = DaemonClient::new();
        Self {
            paths,
            catalog,
            daemon,
        }
    }

    pub(crate) fn paths(&self) -> &Paths {
        &self.paths
    }

    pub(crate) fn config(&self) -> anyhow::Result<Config> {
        Config::load(&self.paths.config_file)
    }

    pub(crate) fn catalog(&self) -> &ProviderCatalog {
        &self.catalog
    }

    pub(crate) fn daemon(&self) -> &DaemonClient {
        &self.daemon
    }

    /// The single mount-enumeration funnel used by every command.
    ///
    /// Reads one `Spec` per JSON file in the `mounts/` directory and returns
    /// the list sorted by mount name.
    pub(crate) fn mounts(&self) -> anyhow::Result<Vec<MountConfig>> {
        let mut configs = per_file_mount_paths(&self.paths.mounts_dir)?
            .into_iter()
            .map(|path| MountConfig::from_path(&path))
            .collect::<anyhow::Result<Vec<_>>>()?;
        configs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(configs)
    }
}

/// Read the per-file mount spec paths from `mounts_dir`.
///
/// Returns an empty list when the directory does not exist (not an error).
pub(crate) fn per_file_mount_paths(mounts_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    omnifs_mount::mounts::spec_paths_in(mounts_dir)
        .with_context(|| format!("read mount config directory {}", mounts_dir.display()))
}
