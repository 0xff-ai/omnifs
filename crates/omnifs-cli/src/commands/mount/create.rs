//! Narrow Setup compatibility helper.
//!
//! Setup still builds its no-sign-in quick-start definition in the legacy
//! mount wire shape until its Plan 008 resource rewrite lands. Public mount
//! commands do not use this module.

use anyhow::Context as _;
use omnifs_api::{MountDefinition, MountLimits};
use omnifs_core::ProviderId;
use omnifs_provider::ProviderManifest;
use serde::Serialize;

use super::spec_creation::create_config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MountInitStatus {
    Ready,
    SignInDeclined,
}

impl MountInitStatus {
    pub(crate) const fn verdict(self) -> crate::ui::output::ResultVerdict {
        match self {
            Self::Ready => crate::ui::output::ResultVerdict::Ok,
            Self::SignInDeclined => crate::ui::output::ResultVerdict::Degraded,
        }
    }
}

/// Build the temporary legacy shape used only by Setup's quick-start path.
pub(crate) fn quick_start_definition(
    output: &crate::ui::output::Output,
    provider_id: ProviderId,
    manifest: &ProviderManifest,
    mounts: &[omnifs_api::MountRecord],
) -> anyhow::Result<MountDefinition> {
    let mount_name = super::provider_selection::mount_name(
        mounts,
        &manifest.default_mount,
        None,
        false,
        true,
        output,
        crate::auth::auth_receipt_key_width(),
    )?;
    let config = create_config(manifest, output, false)?
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    Ok(MountDefinition {
        name: mount_name,
        provider: provider_id,
        auth: None,
        limits: manifest_limits(manifest),
        config: serde_json::to_vec(&config).context("encode provider config")?,
    })
}

fn manifest_limits(manifest: &ProviderManifest) -> Option<MountLimits> {
    (!manifest.limits.is_empty()).then(|| MountLimits {
        max_memory_mb: manifest
            .limits
            .max_memory_mb
            .as_ref()
            .map(|limit| limit.value),
        max_fetch_blob_bytes: manifest
            .limits
            .max_fetch_blob_bytes
            .as_ref()
            .map(|limit| limit.value),
    })
}
