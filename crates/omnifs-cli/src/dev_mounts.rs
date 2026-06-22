//! Built-in contributor dev mounts embedded at compile time from
//! `providers/*/dev-mount.json`.
//!
//! A dev mount is authored as a *seed*: a provider NAME plus mount config. At
//! `omnifs dev` time each seed is resolved against the content-addressed store
//! to a pinned [`Spec`], so the embedded JSON never carries a content id.

use anyhow::Context;

use omnifs_core::ProviderName;
use omnifs_mount::mounts::Spec;
use omnifs_mount::{Auth, ProviderConfig};
use serde::Deserialize;

use crate::catalog::ProviderTemplates;

include!(concat!(env!("OUT_DIR"), "/embedded_dev_mounts.rs"));

/// A contributor dev-mount seed: a provider name plus mount config, resolved to
/// a pinned [`Spec`] at launch time.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DevMountSeed {
    /// Provider NAME (e.g. `github`), not a wasm filename.
    pub provider: ProviderName,
    pub mount: String,
    #[serde(default, deserialize_with = "omnifs_mount::deserialize_mount_auth")]
    pub auth: Vec<Auth>,
    #[serde(default, rename = "config")]
    pub config: Option<ProviderConfig>,
}

impl DevMountSeed {
    /// Parse a seed from a `dev-mount.json` document.
    pub(crate) fn parse(json: &str) -> anyhow::Result<Self> {
        serde_json::from_str(json).context("parse dev mount seed")
    }

    /// Pin this seed against the installed templates, producing a runnable
    /// [`Spec`]. Returns `None` when the named provider is not installed.
    pub(crate) fn pin(&self, templates: &ProviderTemplates) -> Option<Spec> {
        let template = templates.by_id(self.provider.as_str())?;
        // Seed explicit grants from the manifest's needs, like `omnifs init`:
        // the manifest never grants at runtime, so a dev mount must carry its
        // own grants or the required-capabilities check rejects it.
        let capabilities = (!template.manifest.capabilities.is_empty())
            .then(|| template.manifest.provider_capabilities());
        Some(Spec {
            provider: template.reference.clone(),
            mount: self.mount.clone(),
            root_mount: false,
            auth: self.auth.clone(),
            capabilities,
            config_raw: self.config.clone(),
        })
    }
}

/// Parse the embedded dev-mount seeds, paired with their source filename. They
/// are pinned and pushed to the daemon; nothing is written to disk here.
pub(crate) fn seeds() -> anyhow::Result<Vec<(String, DevMountSeed)>> {
    let mut seeds: Vec<(String, DevMountSeed)> = EMBEDDED_DEV_MOUNTS
        .iter()
        .map(|(filename, json)| {
            let seed = DevMountSeed::parse(json)
                .with_context(|| format!("parse embedded dev mount {filename}"))?;
            Ok(((*filename).to_string(), seed))
        })
        .collect::<anyhow::Result<_>>()?;
    // TEMP(cap-rework): skip the db dev mount while testing the capability-model
    // rework. Its `/data` preopen is a Docker volume fixture, not a host path,
    // and the new grant model does not yet pass it through unrewritten; that
    // handling lands in a separate PR. Remove this `retain` when it does.
    seeds.retain(|(_, seed)| seed.provider.as_str() != "db");
    Ok(seeds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_dev_mounts_exclude_fixture_provider() {
        assert!(
            EMBEDDED_DEV_MOUNTS
                .iter()
                .all(|(filename, _)| *filename != "test.json"),
            "test-provider is a fixture and must not be installed by omnifs dev"
        );
        for (_, seed) in seeds().expect("embedded dev mounts must parse") {
            assert_ne!(
                seed.provider.as_str(),
                "test-provider",
                "test-provider must not be auto-mounted"
            );
        }
    }

    #[test]
    fn embedded_dev_mounts_parse() {
        let seeds = seeds().expect("embedded dev mounts must parse");
        assert!(!seeds.is_empty());
    }

    #[test]
    fn kubernetes_is_not_auto_mounted() {
        // kubernetes needs a live cluster, so its dev mount lives under
        // `testenv/` and is injected by `omnifs dev`'s testenv flow, never
        // embedded into the always-on set.
        for (filename, seed) in seeds().expect("embedded dev mounts must parse") {
            assert_ne!(filename, "k8s.json");
            assert_ne!(seed.provider.as_str(), "kubernetes");
        }
    }
}
