//! Workspace-owned configuration and frontend image assets.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub system: System,
    pub metrics: Metrics,
    pub frontend: FrontendAssets,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct System {
    pub frontend_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Metrics {
    pub enabled: bool,
}

impl Default for Metrics {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FrontendAssets {
    pub guest_image: Option<String>,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            },
        };
        let text = std::str::from_utf8(&bytes).map_err(|error| ConfigError::Parse {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        deserialize(text, path)
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O for {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config at {path}: {message}")]
    Parse { path: PathBuf, message: String },
}

fn deserialize<T: DeserializeOwned>(text: &str, path: &Path) -> Result<T, ConfigError> {
    toml::from_str(text).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_load_preserves_assets_and_rejects_removed_frontend_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[system]\nfrontend_image = \"example/frontend\"\n[metrics]\nenabled = false\n[frontend]\nguest_image = \"guest.ext4\"\n",
        )
        .unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.system.frontend_image.as_deref(),
            Some("example/frontend")
        );
        assert!(!config.metrics.enabled);
        assert_eq!(config.frontend.guest_image.as_deref(), Some("guest.ext4"));

        std::fs::write(&path, "[[frontends]]\nfilesystem = \"fuse\"\n").unwrap();
        let error = Config::load(&path).unwrap_err().to_string();
        assert!(error.contains("frontends"));
    }

    #[test]
    fn config_load_reports_invalid_utf8_and_defaults_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(Config::load(&path).unwrap().metrics.enabled);

        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        let error = Config::load(&path).unwrap_err().to_string();
        assert!(error.contains("invalid config"));
    }
}
