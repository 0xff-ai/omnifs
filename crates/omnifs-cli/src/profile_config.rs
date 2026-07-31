//! Strict, non-filesystem CLI preferences stored in a profile.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProfileConfig {
    pub(crate) metrics: MetricsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct MetricsConfig {
    pub(crate) enabled: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

pub(crate) fn read(root: &Path) -> anyhow::Result<ProfileConfig> {
    let path = root.join("config.toml");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProfileConfig::default());
        },
        Err(error) => return Err(error.into()),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| anyhow::anyhow!("parse config {}: {error}", path.display()))?;
    toml::from_str(text)
        .map_err(|error| anyhow::anyhow!("parse config {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_absent_and_rejects_retired_filesystem_settings() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).unwrap().metrics.enabled);
        assert!(!dir.path().join("config.toml").exists());

        std::fs::write(
            dir.path().join("config.toml"),
            "[filesystem]\ndocker_image = \"old\"\n",
        )
        .unwrap();
        let error = read(dir.path()).unwrap_err().to_string();
        assert!(error.contains("unknown field"), "{error}");
        assert!(error.contains("filesystem"), "{error}");
    }
}
