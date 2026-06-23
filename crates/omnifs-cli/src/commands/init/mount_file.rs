use super::config_generation::GeneratedMountConfig;
use crate::auth::AuthSelection;
use anyhow::Context;
use omnifs_caps::Grants;
use omnifs_core::{AuthKind, MountName, ProviderRef};
use omnifs_mount::mounts::Spec;
use omnifs_mount::{Auth, OAuth, ProviderConfig, StaticToken};
use serde::Serialize;
use std::fs;
use std::path::Path;

pub(super) struct MountFile<'a> {
    mount_name: &'a MountName,
    /// The pinned provider reference written into the mount spec, taken from the
    /// latest installed artifact for this provider.
    reference: &'a ProviderRef,
    auth: Option<&'a AuthSelection>,
    scopes: &'a [String],
    generated: GeneratedMountConfig,
}

impl<'a> MountFile<'a> {
    pub(super) fn new(
        mount_name: &'a MountName,
        reference: &'a ProviderRef,
        auth: Option<&'a AuthSelection>,
        scopes: &'a [String],
        generated: GeneratedMountConfig,
    ) -> Self {
        Self {
            mount_name,
            reference,
            auth,
            scopes,
            generated,
        }
    }

    pub(super) fn write_to(&self, path: &Path) -> anyhow::Result<()> {
        let config = self.serializable();
        let pretty = serde_json::to_string_pretty(&config).context("serialize mount config")?;
        fs::write(path, format!("{pretty}\n"))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub(super) fn into_spec(self) -> Spec {
        Spec {
            provider: self.reference.clone(),
            mount: self.mount_name.to_string(),
            root_mount: false,
            auth: self.auth.map_or_else(Vec::new, |auth| {
                let account = auth.account.clone();
                let scheme = auth.scheme.clone();
                match auth.auth_type {
                    AuthKind::StaticToken => vec![Auth::StaticToken(StaticToken {
                        scheme,
                        account,
                        ..StaticToken::default()
                    })],
                    AuthKind::OAuth => vec![Auth::OAuth(OAuth {
                        scheme,
                        account,
                        scopes: (!self.scopes.is_empty()).then(|| self.scopes.to_vec()),
                        ..OAuth::default()
                    })],
                }
            }),
            capabilities: self.generated.capabilities,
            config_raw: self.generated.config.map(ProviderConfig::from_value),
        }
    }

    fn serializable(&self) -> SerializableMountFile<'_> {
        SerializableMountFile {
            provider: self.reference,
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

/// Typed mount config persisted into the resolved omnifs config file.
///
/// Field declaration order is the serialization order; using a struct
/// instead of an ad-hoc `serde_json::Map` removes the need for
/// `serde_json`'s `preserve_order` feature.
#[derive(Serialize)]
struct SerializableMountFile<'a> {
    provider: &'a ProviderRef,
    mount: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth: Option<MountAuthEntry<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<Grants>,
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
