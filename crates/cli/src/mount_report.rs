//! Mount and provider scan results shared by status, doctor, and the catalog.

use omnifs_creds::CredentialStore;
use omnifs_model::MountName;
use std::path::{Path, PathBuf};

use crate::auth::AuthReadiness;
use crate::catalog::{LoadedMount, ProviderCatalog, ProviderTemplate};
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
    pub(crate) fn load_mount_by_name(&self, name: &MountName) -> anyhow::Result<LoadedMount> {
        self.load_mount(&self.mount_config_path(name))
    }

    pub(crate) fn mount_config_path(&self, name: &MountName) -> PathBuf {
        crate::paths::mount_config_path_for(self.mounts_dir(), name)
    }

    pub(crate) fn closest_mount_name(&self, target: &str) -> anyhow::Result<Option<String>> {
        let mut best: Option<(usize, String)> = None;
        for path in self.mount_config_paths()? {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str().map(str::to_owned)) else {
                continue;
            };
            let distance = strsim::damerau_levenshtein(target, &stem);
            if distance <= target.len() / 2 + 1 {
                match &best {
                    Some((current, _)) if *current <= distance => {},
                    _ => best = Some((distance, stem)),
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
        for path in self.mount_config_paths()? {
            let loaded = match self.load_mount(&path) {
                Ok(loaded) => loaded,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "skipping unparsable mount config");
                    continue;
                },
            };
            let mount_name = &loaded.config.mount;
            let provider_id = loaded.config.provider_id();
            if templates.contains_key(provider_id) {
                by_provider.insert(provider_id.to_owned(), mount_name.clone());
                continue;
            }
            let provider_file = &loaded.config.provider;
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
        if !self.mounts_dir().exists() {
            return Ok(Vec::new());
        }

        let files = self.mount_config_paths()?;
        let mut providers = Vec::with_capacity(files.len());
        for config_path in files {
            match self.load_mount(&config_path) {
                Ok(loaded) => {
                    let config = loaded.config;
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
        if !self.mounts_dir().exists() {
            return Ok(Vec::new());
        }

        let files = self.mount_config_paths()?;
        let mut mounts = Vec::with_capacity(files.len());
        for config_path in files {
            match self.read_user_mount_status(&config_path, store) {
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
        config_path: &Path,
        store: &dyn CredentialStore,
    ) -> anyhow::Result<UserMountReadyStatus> {
        let loaded = self.load_mount(config_path)?;
        let config = loaded.config;
        let provider_path = self.provider_path(&config);
        let provider_present = provider_path.exists();
        let metadata_available = true;
        let auth = AuthReadiness::from_config(&config, store);
        Ok(UserMountReadyStatus {
            config_path: config_path.to_path_buf(),
            mount: config.mount,
            provider: config.provider,
            provider_present,
            metadata_available,
            auth,
        })
    }
}
