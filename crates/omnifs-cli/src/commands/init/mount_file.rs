use super::config_generation::GeneratedMountConfig;
use crate::auth::AuthSelection;
use anyhow::Context;
use omnifs_core::{AuthKind, MountName};
use omnifs_mount::ProviderConfig;
use omnifs_mount::mounts::Spec;
use omnifs_provider::{ProviderCapabilities, ProviderManifest};
use serde::Serialize;
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::path::Path;

pub(super) struct MountFile<'a> {
    mount_name: &'a MountName,
    manifest: &'a ProviderManifest,
    auth: Option<&'a AuthSelection>,
    scopes: &'a [String],
    generated: GeneratedMountConfig,
}

impl<'a> MountFile<'a> {
    pub(super) fn new(
        mount_name: &'a MountName,
        manifest: &'a ProviderManifest,
        auth: Option<&'a AuthSelection>,
        scopes: &'a [String],
        generated: GeneratedMountConfig,
    ) -> Self {
        Self {
            mount_name,
            manifest,
            auth,
            scopes,
            generated,
        }
    }

    #[cfg(test)]
    pub(super) fn write_to(self, path: &Path) -> anyhow::Result<()> {
        let config = self.serializable();
        let pretty = serde_json::to_string_pretty(&config).context("serialize mount config")?;
        fs::write(path, format!("{pretty}\n"))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub(super) fn into_spec(self) -> anyhow::Result<Spec> {
        let value = serde_json::to_value(self.serializable()).context("serialize mount config")?;
        serde_json::from_value(value).context("deserialize generated mount config")
    }

    fn serializable(&self) -> SerializableMountFile<'_> {
        SerializableMountFile {
            provider: &self.manifest.provider,
            mount: self.mount_name.as_str(),
            auth: self.auth.map(|auth| MountAuthEntry {
                auth_type: auth.auth_type,
                scheme: auth.scheme.as_deref(),
                account: auth.account.as_deref(),
                scopes: self.scopes,
            }),
            capabilities: self.generated.capabilities.clone(),
            config: self
                .generated
                .config
                .as_ref()
                .map(|value| ProviderConfig::from_value(value.clone())),
        }
    }
}

/// Typed mount config persisted into `~/.omnifs/config.toml`.
///
/// Field declaration order is the serialization order; using a struct
/// instead of an ad-hoc `serde_json::Map` removes the need for
/// `serde_json`'s `preserve_order` feature.
#[derive(Serialize)]
struct SerializableMountFile<'a> {
    provider: &'a str,
    mount: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth: Option<MountAuthEntry<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<ProviderCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<ProviderConfig>,
}

#[derive(Serialize)]
struct MountAuthEntry<'a> {
    #[serde(rename = "type")]
    auth_type: AuthKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    scheme: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<&'a str>,
    #[serde(skip_serializing_if = "slice_is_empty")]
    scopes: &'a [String],
}

fn slice_is_empty(s: &&[String]) -> bool {
    s.is_empty()
}
