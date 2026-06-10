//! Mount and provider scan results shared by status, doctor, and the catalog.

use omnifs_core::MountName;
use omnifs_creds::CredentialStore;
use std::path::PathBuf;

use crate::auth::AuthReadiness;
use crate::catalog::{ProviderCatalog, ProviderTemplate};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderReadyStatus {
    pub(crate) config_path: PathBuf,
    pub(crate) mount: String,
    pub(crate) provider: String,
    pub(crate) provider_present: bool,
    pub(crate) metadata_available: bool,
    pub(crate) root_mount: bool,
    pub(crate) auth_count: usize,
    pub(crate) domain_count: usize,
    pub(crate) git_repo_count: usize,
    pub(crate) max_memory_mb: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderConfigStatus {
    Ready(ProviderReadyStatus),
    Invalid { config_path: PathBuf, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UserMountStatus {
    Ready(UserMountReadyStatus),
    Invalid { config_path: PathBuf, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserMountReadyStatus {
    pub(crate) config_path: PathBuf,
    pub(crate) mount: String,
    pub(crate) provider: String,
    pub(crate) provider_present: bool,
    pub(crate) metadata_available: bool,
    pub(crate) auth: AuthReadiness,
}

impl ProviderCatalog {
    pub(crate) fn load_mount_by_name(
        &self,
        name: &MountName,
    ) -> anyhow::Result<omnifs_mount_schema::mounts::Resolved> {
        let mount = self
            .session_mount_configs()?
            .into_iter()
            .find(|mount| &mount.name == name)
            .ok_or_else(|| anyhow::anyhow!("no mount config named `{name}`"))?;
        self.resolve_mount_spec(mount.config, true)
    }

    pub(crate) fn closest_mount_name(&self, target: &str) -> anyhow::Result<Option<String>> {
        let mut best: Option<(usize, String)> = None;
        for mount in self.session_mount_configs()? {
            let name = mount.name.to_string();
            let distance = strsim::damerau_levenshtein(target, &name);
            if distance <= target.len() / 2 + 1 {
                match &best {
                    Some((current, _)) if *current <= distance => {},
                    _ => best = Some((distance, name)),
                }
            }
        }
        Ok(best.map(|(_, stem)| stem))
    }

    pub(crate) fn configured_mounts_by_provider(
        &self,
        templates: &BTreeMap<String, ProviderTemplate>,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let mut by_provider: BTreeMap<String, String> = BTreeMap::new();
        for configured in self.session_mount_configs()? {
            let mount = match self.resolve_mount_spec(configured.config.clone(), true) {
                Ok(mount) => mount,
                Err(error) => {
                    tracing::warn!(source = %configured.source.display(), %error, "skipping unparsable mount config");
                    continue;
                },
            };
            let mount_name = &mount.mount;
            let provider_id = mount.provider_id();
            if templates.contains_key(provider_id) {
                by_provider.insert(provider_id.to_owned(), mount_name.clone());
                continue;
            }
            let provider_file = &mount.provider;
            if let Some((id, _)) = templates
                .iter()
                .find(|(_, tmpl)| tmpl.manifest.provider == provider_file.as_str())
            {
                by_provider.insert(id.clone(), mount_name.clone());
            }
        }
        Ok(by_provider)
    }

    pub(crate) fn scan_provider_configs(&self) -> anyhow::Result<Vec<ProviderConfigStatus>> {
        let configs = self.session_mount_configs()?;
        let mut providers = Vec::with_capacity(configs.len());
        for configured in configs {
            let config_path = configured.source.clone();
            match self.resolve_mount_spec(configured.config, true) {
                Ok(config) => {
                    let provider_path = self.provider_path(&config);
                    let provider_present = provider_path.exists();
                    let metadata_available = true;
                    providers.push(ProviderConfigStatus::Ready(ProviderReadyStatus {
                        config_path,
                        mount: config.mount,
                        provider: config.provider,
                        provider_present,
                        metadata_available,
                        root_mount: config.root_mount,
                        auth_count: config.auth.len(),
                        domain_count: config
                            .capabilities
                            .as_ref()
                            .and_then(|caps| caps.domains.as_ref())
                            .map_or(0, Vec::len),
                        git_repo_count: config
                            .capabilities
                            .as_ref()
                            .and_then(|caps| caps.git_repos.as_ref())
                            .map_or(0, Vec::len),
                        max_memory_mb: config.capabilities.and_then(|caps| caps.max_memory_mb),
                    }));
                },
                Err(error) => providers.push(ProviderConfigStatus::Invalid {
                    config_path,
                    error: error.to_string(),
                }),
            }
        }

        Ok(providers)
    }

    pub(crate) fn scan_user_mount_configs(
        &self,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<Vec<UserMountStatus>> {
        let configs = self.session_mount_configs()?;
        let mut mounts = Vec::with_capacity(configs.len());
        for configured in configs {
            let config_path = configured.source.clone();
            match self.read_user_mount_status(configured, store) {
                Ok(status) => mounts.push(UserMountStatus::Ready(status)),
                Err(error) => mounts.push(UserMountStatus::Invalid {
                    config_path,
                    error: error.to_string(),
                }),
            }
        }
        Ok(mounts)
    }

    fn read_user_mount_status(
        &self,
        configured: crate::session::MountConfig,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<UserMountReadyStatus> {
        let config_path = configured.source.clone();
        let config = self.resolve_mount_spec(configured.config, true)?;
        let provider_path = self.provider_path(&config);
        let provider_present = provider_path.exists();
        let metadata_available = true;
        let auth = AuthReadiness::from_config(&config, store);
        Ok(UserMountReadyStatus {
            config_path: config_path.clone(),
            mount: config.mount,
            provider: config.provider,
            provider_present,
            metadata_available,
            auth,
        })
    }
}
