use crate::catalog::mount_exists;
use crate::error::WithHint;
use crate::mount_config::MountConfig;
use anyhow::anyhow;
use omnifs_workspace::mounts::Name as MountName;
use omnifs_workspace::provider::{Provider, ProviderManifest};

pub(crate) struct ProviderSelection<'a> {
    mounts: &'a [MountConfig],
    installed: &'a [(Provider, ProviderManifest)],
}

impl<'a> ProviderSelection<'a> {
    pub(crate) fn new(
        mounts: &'a [MountConfig],
        installed: &'a [(Provider, ProviderManifest)],
    ) -> Self {
        Self { mounts, installed }
    }

    pub(crate) fn provider_names(&self) -> Vec<String> {
        let mut manifests: Vec<&ProviderManifest> = self
            .installed
            .iter()
            .map(|(_, manifest)| manifest)
            .collect();
        manifests.sort_by(|a, b| a.id.cmp(&b.id));
        manifests
            .into_iter()
            .map(|manifest| manifest.id.clone())
            .collect()
    }

    /// The pinned provider's manifest for a name slug, if installed.
    fn manifest_for(&self, name: &str) -> Option<&ProviderManifest> {
        crate::catalog::find_installed(self.installed, name).map(|(_, manifest)| manifest)
    }

    pub(crate) fn resolve(
        &self,
        provider_arg: Option<&str>,
        explicit_name: Option<&str>,
        interactive: bool,
        yes: bool,
        session: &mut crate::ui::session::Session,
    ) -> anyhow::Result<(String, MountName)> {
        let provider = self.resolve_provider(provider_arg, interactive)?;
        // An unknown positional provider bails here (before the caller's own
        // catalog lookup), so the available-provider list and install hint must
        // ride on this error or they never reach the user.
        let manifest = self
            .manifest_for(&provider)
            .ok_or_else(|| {
                anyhow!(
                    "provider `{provider}` not found; available: {}",
                    self.provider_names().join(", ")
                )
            })
            .with_hint("Run `omnifs provider ls` to list installed providers")
            .with_hint(
                "Or run `omnifs provider add <wasm-or-dir>` to install a provider artifact",
            )?;

        let proposed = explicit_name.map_or_else(|| manifest.default_mount.clone(), str::to_string);
        let proposed_name = MountName::new(proposed.as_str())?;

        // An explicit --as collision is the provider upgrade/re-consent path;
        // the caller rejects a same-artifact collision before auth or config
        // side effects. Accidental default-name collisions still go through
        // the unique-name flow below.
        if explicit_name.is_some() {
            return Ok((provider, proposed_name));
        }

        let name = self.ensure_unique_name(proposed_name, interactive, yes, session)?;
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
        crate::ui::prompt::Select::new("Which provider does this mount use?")
            .items(providers)
            .ask()
    }

    fn ensure_unique_name(
        &self,
        proposed: MountName,
        interactive: bool,
        yes: bool,
        session: &mut crate::ui::session::Session,
    ) -> anyhow::Result<MountName> {
        if !mount_exists(self.mounts, &proposed) {
            return Ok(proposed);
        }
        let suggestion = self.next_available(&proposed)?;
        // `--yes` accepts the auto-suggested name on collision, even
        // non-interactively (it never overwrites the existing mount).
        if yes {
            session.row(crate::ui::report::Row::new(
                crate::ui::style::Glyph::Warn,
                "mount name",
                format!("{proposed} taken, using {suggestion}"),
            ));
            return Ok(suggestion);
        }
        if !interactive {
            anyhow::bail!(
                "mount `{proposed}` already exists; pass --as explicitly (suggested: `{suggestion}`)"
            );
        }
        let name = crate::ui::prompt::Text::new("New mount name")
            .with_default(suggestion.as_str())
            .ask()?;
        Ok(MountName::new(name)?)
    }

    fn next_available(&self, base: &MountName) -> anyhow::Result<MountName> {
        (2..1000)
            .filter_map(|n| MountName::new(format!("{base}-{n}")).ok())
            .find(|candidate| !mount_exists(self.mounts, candidate))
            .ok_or_else(|| anyhow!("could not find an available mount name derived from `{base}`"))
    }
}
