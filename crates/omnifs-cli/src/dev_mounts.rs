//! Built-in contributor dev mounts embedded at compile time from
//! `providers/*/dev-mount.json` (or `{provider, mount}` defaults from the manifest).

use std::fs;
use std::path::Path;

use anyhow::Context;

use crate::session::{MountConfig, Session};

include!(concat!(env!("OUT_DIR"), "/embedded_dev_mounts.rs"));

pub(crate) fn install(session: &Session) -> anyhow::Result<Vec<MountConfig>> {
    install_dir(session.mounts_dir())?;
    EMBEDDED_DEV_MOUNTS
        .iter()
        .map(|(filename, _)| MountConfig::from_path(&session.mounts_dir().join(filename)))
        .collect()
}

pub(crate) fn install_dir(dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    for (filename, json) in EMBEDDED_DEV_MOUNTS {
        let path = dir.join(filename);
        fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
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
}
