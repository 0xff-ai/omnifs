//! Command-facing view of the resolved omnifs home.
//!
//! `Workspace` owns the local layout for one CLI invocation and is the factory
//! for command-scoped handles derived from that layout: config, provider
//! catalog, daemon client, and configured mounts.

use anyhow::Context as _;
use omnifs_home::{Cli as CliRole, Workspace as HomeWorkspace, WorkspaceLayout};
use omnifs_mount::mounts::Registry;
use omnifs_provider::Catalog;
use std::path::{Path, PathBuf};

use crate::client::DaemonClient;
use crate::config::Config;
use crate::credential_target::CredentialTarget;
use crate::session::MountConfig;

/// Resolved local omnifs home for one CLI command.
pub(crate) struct Workspace {
    home: HomeWorkspace<CliRole>,
    catalog: Catalog,
    daemon: DaemonClient,
}

#[derive(Debug, Clone)]
pub(crate) struct MountRemovalTarget {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) credential: CredentialTarget,
}

impl Workspace {
    pub(crate) fn resolve() -> anyhow::Result<Self> {
        let layout = crate::dev_support::contributor_layout()?;
        Ok(Self::from_layout(layout))
    }

    pub(crate) fn from_layout(layout: WorkspaceLayout) -> Self {
        Self::from_home(HomeWorkspace::from_layout(layout))
    }

    pub(crate) fn from_home(home: HomeWorkspace<CliRole>) -> Self {
        let catalog = Catalog::open(home.providers_dir());
        let daemon = DaemonClient::new();
        Self {
            home,
            catalog,
            daemon,
        }
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
        let registry = Registry::load(self.home.mounts_dir())?;
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
    /// Enumerates the per-file spec paths directly and tolerates unparsable
    /// files: a broken JSON file still produces a removal target with
    /// `CredentialTarget::None` so reset can nuke broken state.
    pub(crate) fn reset_removal_targets(&self) -> anyhow::Result<Vec<MountRemovalTarget>> {
        use omnifs_mount::mounts::Spec as MountSpec;

        let mut targets = Vec::new();
        let paths = per_file_mount_paths(self.home.mounts_dir())?;
        for path in paths {
            let Some(name) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let credential = match MountSpec::from_file(&path) {
                Ok(spec) => match crate::catalog::resolve_mount_spec(&self.catalog, &spec, false) {
                    Ok(resolved) => CredentialTarget::for_mount(&resolved),
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "unresolvable mount config; will remove the file but cannot drop credentials"
                        );
                        CredentialTarget::None
                    },
                },
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "unparsable mount config; will remove the file but cannot drop credentials"
                    );
                    CredentialTarget::None
                },
            };
            targets.push(MountRemovalTarget {
                name,
                path,
                credential,
            });
        }

        targets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(targets)
    }
}

/// Read the per-file mount spec paths from `mounts_dir`.
///
/// Returns an empty list when the directory does not exist (not an error).
pub(crate) fn per_file_mount_paths(mounts_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    omnifs_mount::mounts::spec_paths_in(mounts_dir)
        .with_context(|| format!("read mount config directory {}", mounts_dir.display()))
}
