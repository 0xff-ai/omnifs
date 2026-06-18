//! Global `config.toml` loader. Lives at `paths.config_file`.
//!
//! Resolution order is: CLI flag > env var > config file > built-in default.
//! Missing file is not an error; malformed file is. The Config is loaded
//! once and threaded into commands that need it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    /// Daemon launch backend, recorded by `omnifs setup`. Unset falls back to
    /// the platform default (host-native on macOS, Docker elsewhere).
    pub runtime: Option<Runtime>,
}

/// Daemon launch backend. `omnifs setup` records the choice; `omnifs up`
/// reads it and starts the daemon that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    /// Daemon runs inside a Docker container; the CLI owns the container
    /// lifecycle.
    Docker,
    /// Daemon runs host-native as a child process serving the NFS frontend.
    Native,
}

impl Runtime {
    /// Default when `setup` has recorded nothing: host-native on macOS (no
    /// Docker assumption), Docker elsewhere.
    pub fn platform_default() -> Self {
        if cfg!(target_os = "macos") {
            Self::Native
        } else {
            Self::Docker
        }
    }
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

    /// Daemon launch backend: the recorded `[system].runtime`, or the platform
    /// default when `setup` has not chosen one.
    pub fn runtime(&self) -> Runtime {
        self.system
            .runtime
            .unwrap_or_else(Runtime::platform_default)
    }
}

pub struct ConfigFile {
    path: PathBuf,
    doc: toml::Value,
}

impl ConfigFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let doc = match std::fs::read_to_string(&path) {
            Ok(raw) => raw
                .parse::<toml::Value>()
                .with_context(|| format!("parse {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                toml::Value::Table(toml::map::Map::new())
            },
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        Ok(Self { path, doc })
    }

    /// Set `[system].runtime`, preserving the rest of the config. `omnifs setup`
    /// records the launch backend here so `omnifs up` reads it.
    pub fn set_system_runtime(&mut self, runtime: Runtime) -> Result<()> {
        let root = self
            .doc
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("{} is not a TOML table", self.path.display()))?;
        let system = root
            .entry("system".to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let system = system.as_table_mut().ok_or_else(|| {
            anyhow::anyhow!("{} has a non-table [system] value", self.path.display())
        })?;
        let value = toml::Value::try_from(runtime).context("serialize runtime as TOML")?;
        system.insert("runtime".to_string(), value);
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let rendered = toml::to_string_pretty(&self.doc).context("serialize config TOML")?;
        std::fs::write(&self.path, rendered)
            .with_context(|| format!("write {}", self.path.display()))?;
        Ok(())
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

    // Guards the on-disk `[system].runtime` token: `setup` writes it and `up`
    // reads it, so a rename of the serialized form would silently break the
    // runtime selection across a CLI upgrade.
    #[test]
    fn runtime_round_trips_through_config_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut file = ConfigFile::load(&path).unwrap();
        file.set_system_runtime(Runtime::Native).unwrap();
        file.save().unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("runtime = \"native\""), "got:\n{raw}");

        let config = Config::load(&path).unwrap();
        assert_eq!(config.system.runtime, Some(Runtime::Native));
        assert_eq!(config.runtime(), Runtime::Native);
    }
}
