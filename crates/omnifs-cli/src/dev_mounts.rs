//! Built-in contributor dev mounts embedded at compile time from
//! `providers/*/dev-mount.json` (or `{provider, mount}` defaults from the manifest).

use std::path::PathBuf;

use anyhow::Context;
use omnifs_mount::mounts::Spec;

use crate::session::MountConfig;

include!(concat!(env!("OUT_DIR"), "/embedded_dev_mounts.rs"));

/// Parse the embedded dev mount specs. They are pushed to the daemon over
/// the control API like any other mount; nothing is written to disk.
pub(crate) fn configs() -> anyhow::Result<Vec<MountConfig>> {
    EMBEDDED_DEV_MOUNTS
        .iter()
        .map(|(filename, json)| {
            let spec = Spec::parse(json)
                .with_context(|| format!("parse embedded dev mount {filename}"))?;
            MountConfig::from_parsed(
                spec,
                PathBuf::from(format!("embedded dev mount {filename}")),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_dev_mounts_exclude_fixture_provider() {
        assert!(
            EMBEDDED_DEV_MOUNTS
                .iter()
                .all(|(filename, json)| *filename != "test.json"
                    && !json.contains("test_provider.wasm")),
            "test-provider is a fixture and must not be installed by omnifs dev"
        );
    }

    #[test]
    fn embedded_dev_mounts_parse() {
        let configs = configs().expect("embedded dev mounts must parse");
        assert!(!configs.is_empty());
    }

    #[test]
    fn kubernetes_is_not_auto_mounted() {
        // kubernetes needs a live cluster, so its dev mount lives under
        // `testenv/` and is injected by `omnifs dev`'s testenv flow, never
        // embedded into the always-on set.
        assert!(
            EMBEDDED_DEV_MOUNTS
                .iter()
                .all(|(filename, json)| *filename != "k8s.json"
                    && !json.contains("omnifs_provider_kubernetes.wasm")),
            "kubernetes must not be auto-mounted by plain omnifs dev"
        );
    }
}
