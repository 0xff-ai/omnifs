//! Command-facing view of the resolved omnifs home.
//!
//! `Workspace` owns the local layout for one CLI invocation and is the factory
//! for command-scoped handles derived from that layout: config, provider
//! catalog, daemon client, and configured mounts.

use omnifs_home::{Cli as CliRole, Workspace as HomeWorkspace, WorkspaceLayout};
use omnifs_mount::mounts::Registry;
use omnifs_provider::Catalog;
use std::path::PathBuf;

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
    /// Reads through the shared [`Registry`]: resolvable specs yield a target
    /// plus their stored credential; files that failed to load (broken JSON,
    /// name/filename mismatch) still produce a target with
    /// `CredentialTarget::None` so reset can nuke broken state.
    pub(crate) fn reset_removal_targets(&self) -> anyhow::Result<Vec<MountRemovalTarget>> {
        let registry = Registry::load(self.home.mounts_dir())?;
        let mut targets = Vec::new();

        for (name, spec) in registry.iter() {
            let credential = match crate::catalog::resolve_mount_spec(&self.catalog, spec, false) {
                Ok(resolved) => CredentialTarget::for_mount(&resolved),
                Err(error) => {
                    tracing::warn!(
                        mount = %name,
                        %error,
                        "unresolvable mount config; will remove the file but cannot drop credentials"
                    );
                    CredentialTarget::None
                },
            };
            targets.push(MountRemovalTarget {
                name: name.to_string(),
                path: registry.spec_path(name),
                credential,
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
                credential: CredentialTarget::None,
            });
        }

        targets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(targets)
    }
}
