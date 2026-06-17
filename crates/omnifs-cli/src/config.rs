//! Global `config.toml` loader. Lives at `paths.config_file`.
//!
//! Resolution order is: CLI flag > env var > config file > built-in default.
//! Missing file is not an error; malformed file is. The Config is loaded
//! once and threaded into commands that need it.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub system: ConfigSystem,
    /// Legacy top-level setting, still accepted for existing config files.
    pub container_name: Option<String>,
    /// Legacy top-level setting, still accepted for existing config files.
    pub image: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigSystem {
    pub container_name: Option<String>,
    pub image: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            },
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", path.display()));
            },
        };
        let mut config: Self =
            toml::from_str(&bytes).with_context(|| format!("parse {}", path.display()))?;
        config.apply_system_section();
        Ok(config)
    }

    fn apply_system_section(&mut self) {
        self.container_name = self
            .container_name
            .clone()
            .or(self.system.container_name.clone());
        self.image = self.image.clone().or(self.system.image.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_section_feeds_legacy_runtime_fields() {
        let config: Config = toml::from_str(
            r#"
                [system]
                container_name = "omnifs-test"
                image = "ghcr.io/example/omnifs:test"
            "#,
        )
        .unwrap();
        let mut config = config;
        config.apply_system_section();

        assert_eq!(config.container_name.as_deref(), Some("omnifs-test"));
        assert_eq!(config.image.as_deref(), Some("ghcr.io/example/omnifs:test"));
    }
}
