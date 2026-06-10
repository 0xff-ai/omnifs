use crate::catalog::{ProviderTemplate, mount_exists};
use crate::session::MountConfig;
use anyhow::anyhow;
use omnifs_core::MountName;
use std::collections::BTreeMap;

pub(super) struct ProviderSelection<'a> {
    mounts: &'a [MountConfig],
    templates: &'a BTreeMap<String, ProviderTemplate>,
}

impl<'a> ProviderSelection<'a> {
    pub(super) fn new(
        mounts: &'a [MountConfig],
        templates: &'a BTreeMap<String, ProviderTemplate>,
    ) -> Self {
        Self { mounts, templates }
    }

    pub(super) fn provider_names(&self) -> Vec<String> {
        let mut providers: Vec<&ProviderTemplate> = self.templates.values().collect();
        providers
            .sort_by_key(|template| (template.source.sort_key(), template.manifest.id.as_str()));
        providers
            .into_iter()
            .map(|template| template.manifest.id.clone())
            .collect()
    }

    pub(super) fn resolve(
        &self,
        provider_arg: Option<&str>,
        explicit_name: Option<&str>,
        interactive: bool,
        yes: bool,
    ) -> anyhow::Result<(String, MountName)> {
        let provider = self.resolve_provider(provider_arg, interactive)?;
        let template = self
            .templates
            .get(&provider)
            .ok_or_else(|| anyhow!("provider `{provider}` not found"))?;

        let proposed =
            explicit_name.map_or_else(|| template.manifest.default_mount.clone(), str::to_string);
        let proposed_name = MountName::new(proposed.as_str())?;

        // Explicit --as collisions always error, regardless of --yes.
        if explicit_name.is_some() {
            if mount_exists(self.mounts, &proposed_name) {
                anyhow::bail!(
                    "mount `{proposed}` already exists; choose a different name with --as"
                );
            }
            return Ok((provider, proposed_name));
        }

        let name = self.ensure_unique_name(proposed_name, interactive, yes)?;
        Ok((provider, name))
    }

    fn resolve_provider(
        &self,
        provider_arg: Option<&str>,
        interactive: bool,
    ) -> anyhow::Result<String> {
        if let Some(provider) = provider_arg {
            return Ok(provider.to_string());
        }
        if !interactive {
            anyhow::bail!("non-interactive mode requires a provider argument");
        }
        let providers = self.provider_names();
        if providers.is_empty() {
            anyhow::bail!("no providers found");
        }
        inquire::Select::new("Which provider does this mount use?", providers)
            .prompt()
            .map_err(|e| anyhow!("prompt error: {e}"))
    }

    fn ensure_unique_name(
        &self,
        proposed: MountName,
        interactive: bool,
        yes: bool,
    ) -> anyhow::Result<MountName> {
        if !mount_exists(self.mounts, &proposed) {
            return Ok(proposed);
        }
        let suggestion = self.next_available(&proposed)?;
        if !interactive {
            anyhow::bail!(
                "mount `{proposed}` already exists; pass --as explicitly (suggested: `{suggestion}`)"
            );
        }
        if yes {
            return Ok(suggestion);
        }
        anstream::println!("Mount `{proposed}` already exists.");
        let name = inquire::Text::new("New mount name")
            .with_default(suggestion.as_str())
            .prompt()
            .map_err(|e| anyhow!("prompt error: {e}"))?;
        Ok(MountName::new(name)?)
    }

    fn next_available(&self, base: &MountName) -> anyhow::Result<MountName> {
        (2..1000)
            .filter_map(|n| MountName::new(format!("{base}-{n}")).ok())
            .find(|candidate| !mount_exists(self.mounts, candidate))
            .ok_or_else(|| anyhow!("could not find an available mount name derived from `{base}`"))
    }
}
