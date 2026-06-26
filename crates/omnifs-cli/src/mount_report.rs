//! Mount and provider scan results shared by status, doctor, and the catalog.

use omnifs_core::MountName;
use omnifs_creds::CredentialStore;
use serde::Serialize;
use std::path::PathBuf;

use crate::auth::AuthReadiness;
use crate::catalog::ProviderCatalog;
use crate::session::MountConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProviderReadyStatus {
    pub(crate) config_path: PathBuf,
    pub(crate) mount: String,
    pub(crate) provider: String,
    pub(crate) provider_present: bool,
    pub(crate) root_mount: bool,
    pub(crate) auth_count: usize,
    pub(crate) domain_count: usize,
    pub(crate) git_repo_count: usize,
    pub(crate) max_memory_mb: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ProviderConfigStatus {
    Ready(ProviderReadyStatus),
    Invalid { config_path: PathBuf, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum UserMountStatus {
    Ready(UserMountReadyStatus),
    Invalid { config_path: PathBuf, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UserMountReadyStatus {
    pub(crate) config_path: PathBuf,
    pub(crate) mount: String,
    pub(crate) provider: String,
    pub(crate) provider_present: bool,
    pub(crate) auth: AuthReadiness,
}

impl ProviderCatalog {
    pub(crate) fn load_mount_by_name(
        &self,
        mounts: &[MountConfig],
        name: &MountName,
    ) -> anyhow::Result<omnifs_mount::mounts::Resolved> {
        let mount = mounts
            .iter()
            .find(|m| &m.name == name)
            .ok_or_else(|| anyhow::anyhow!("no mount config named `{name}`"))?;
        self.resolve_mount_spec(&mount.config, true)
    }

    pub(crate) fn scan_provider_configs(
        &self,
        mounts: Vec<MountConfig>,
    ) -> Vec<ProviderConfigStatus> {
        let mut providers = Vec::with_capacity(mounts.len());
        for configured in mounts {
            let config_path = configured.source.clone();
            match self.resolve_mount_spec(&configured.config, true) {
                Ok(config) => {
                    let provider_path = self.provider_path(&config);
                    let provider_present = provider_path.exists();
                    providers.push(ProviderConfigStatus::Ready(ProviderReadyStatus {
                        config_path,
                        mount: config.spec.mount,
                        provider: config.provider_name.clone(),
                        provider_present,
                        root_mount: config.spec.root_mount,
                        auth_count: config.spec.auth.len(),
                        domain_count: config
                            .spec
                            .capabilities
                            .as_ref()
                            .and_then(|caps| caps.domains.as_ref())
                            .map_or(0, |grant| grant.literal().len()),
                        git_repo_count: config
                            .spec
                            .capabilities
                            .as_ref()
                            .and_then(|caps| caps.git_repos.as_ref())
                            .map_or(0, |grant| grant.literal().len()),
                        max_memory_mb: config.spec.capabilities.and_then(|caps| caps.max_memory_mb),
                    }));
                },
                Err(error) => providers.push(ProviderConfigStatus::Invalid {
                    config_path,
                    error: error.to_string(),
                }),
            }
        }
        providers
    }

    pub(crate) fn scan_user_mount_configs(
        &self,
        mounts: Vec<MountConfig>,
        store: &dyn CredentialStore,
    ) -> Vec<UserMountStatus> {
        let mut statuses = Vec::with_capacity(mounts.len());
        for configured in mounts {
            let config_path = configured.source.clone();
            match self.read_user_mount_status(&configured, store) {
                Ok(status) => statuses.push(UserMountStatus::Ready(status)),
                Err(error) => statuses.push(UserMountStatus::Invalid {
                    config_path,
                    error: error.to_string(),
                }),
            }
        }
        statuses
    }

    fn read_user_mount_status(
        &self,
        configured: &MountConfig,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<UserMountReadyStatus> {
        let config_path = configured.source.clone();
        let config = self.resolve_mount_spec(&configured.config, true)?;
        let provider_path = self.provider_path(&config);
        let provider_present = provider_path.exists();
        let auth = self
            .resolve_mount_auth_tolerating_manifest_errors(config.clone())
            .readiness(store);
        Ok(UserMountReadyStatus {
            config_path: config_path.clone(),
            mount: config.spec.mount,
            provider: config.provider_name.clone(),
            provider_present,
            auth,
        })
    }
}

/// Returns the first mount whose name starts with `target`.
pub(crate) fn closest_mount_name(mounts: &[MountConfig], target: &str) -> Option<String> {
    mounts
        .iter()
        .map(|mount| mount.name.to_string())
        .find(|name| name.starts_with(target))
}
