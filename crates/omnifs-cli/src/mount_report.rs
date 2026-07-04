//! Mount and provider scan results shared by status, doctor, and the catalog.

use omnifs_workspace::creds::CredentialStore;
use omnifs_workspace::mounts::Name as MountName;
use serde::Serialize;
use std::path::PathBuf;

use omnifs_workspace::mounts::Spec;
use omnifs_workspace::provider::Catalog;

use crate::auth::AuthReadiness;
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

pub(crate) fn load_mount_by_name(mounts: &[MountConfig], name: &MountName) -> anyhow::Result<Spec> {
    let mount = mounts
        .iter()
        .find(|m| &m.name == name)
        .ok_or_else(|| anyhow::anyhow!("no mount config named `{name}`"))?;
    Ok(mount.config.clone())
}

/// Whether the spec's pinned provider artifact is installed and its manifest
/// loads. `false` when the artifact is absent; an error when it is present but
/// its manifest is unreadable (a corrupt install), which the scans surface as
/// `Invalid`.
fn artifact_present(catalog: &Catalog, spec: &Spec) -> anyhow::Result<bool> {
    match catalog.get(&spec.provider.id)? {
        Some(provider) => {
            provider.manifest()?;
            Ok(true)
        },
        None => Ok(false),
    }
}

pub(crate) fn scan_provider_configs(
    catalog: &Catalog,
    mounts: Vec<MountConfig>,
) -> Vec<ProviderConfigStatus> {
    let mut providers = Vec::with_capacity(mounts.len());
    for configured in mounts {
        let config_path = configured.source.clone();
        let spec = &configured.config;
        let provider_present = match artifact_present(catalog, spec) {
            Ok(present) => present,
            Err(error) => {
                providers.push(ProviderConfigStatus::Invalid {
                    config_path,
                    error: error.to_string(),
                });
                continue;
            },
        };
        providers.push(ProviderConfigStatus::Ready(ProviderReadyStatus {
            config_path,
            mount: spec.mount.clone(),
            provider: spec.provider_name().to_string(),
            provider_present,
            root_mount: spec.root_mount,
            auth_count: usize::from(spec.auth.is_some()),
            domain_count: spec
                .capabilities
                .as_ref()
                .and_then(|caps| caps.domains.as_ref())
                .map_or(0, |grant| grant.literal().len()),
            git_repo_count: spec
                .capabilities
                .as_ref()
                .and_then(|caps| caps.git_repos.as_ref())
                .map_or(0, |grant| grant.literal().len()),
            max_memory_mb: spec
                .capabilities
                .as_ref()
                .and_then(|caps| caps.max_memory_mb),
        }));
    }
    providers
}

pub(crate) fn scan_user_mount_configs(
    catalog: &Catalog,
    mounts: Vec<MountConfig>,
    store: &dyn CredentialStore,
) -> Vec<UserMountStatus> {
    let mut statuses = Vec::with_capacity(mounts.len());
    for configured in mounts {
        let config_path = configured.source.clone();
        match read_user_mount_status(catalog, &configured, store) {
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
    catalog: &Catalog,
    configured: &MountConfig,
    store: &dyn CredentialStore,
) -> anyhow::Result<UserMountReadyStatus> {
    let spec = &configured.config;
    let provider_present = artifact_present(catalog, spec)?;
    let auth = crate::auth::mount_auth(catalog, spec.clone()).readiness(store);
    Ok(UserMountReadyStatus {
        config_path: configured.source.clone(),
        mount: spec.mount.clone(),
        provider: spec.provider_name().to_string(),
        provider_present,
        auth,
    })
}

/// Returns the first mount whose name starts with `target`.
pub(crate) fn closest_mount_name(mounts: &[MountConfig], target: &str) -> Option<String> {
    mounts
        .iter()
        .map(|mount| mount.name.to_string())
        .find(|name| name.starts_with(target))
}
