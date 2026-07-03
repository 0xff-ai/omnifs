//! Global `config.toml` loader. Lives at `paths.config_file`.
//!
//! Resolution order is: CLI flag > env var > config file > built-in default.
//! Missing file is not an error; malformed file is. Commands load it from
//! their resolved workspace when they need launch or Docker policy.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub system: ConfigSystem,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigSystem {
    pub container_name: Option<String>,
    pub image: Option<String>,
    /// Daemon launch backend, recorded by `omnifs setup`. Unset falls back to
    /// the platform default (host-native).
    pub runtime: Option<ConfiguredBackend>,
}

/// Daemon launch backend. `omnifs setup` records the default choice; `omnifs up`
/// reads it, and `omnifs up --runtime` overrides it for one launch. The
/// `ValueEnum` derive makes `docker`/`native` valid `--runtime` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ConfiguredBackend {
    /// Daemon runs inside a Docker container; the CLI owns the container
    /// lifecycle.
    Docker,
    /// Daemon runs host-native as a child process serving the platform frontend.
    Native,
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
        toml::from_str(&bytes).with_context(|| format!("parse {}", path.display()))
    }

    /// Daemon launch backend: the recorded `[system].runtime`, or the platform
    /// default when `setup` has not chosen one.
    pub fn backend(&self) -> ConfiguredBackend {
        self.system.runtime.unwrap_or(ConfiguredBackend::Native)
    }
}

/// Resolve one setting through the single CLI precedence chain:
/// CLI flag > env var > config file > built-in default.
///
/// The env var is read through [`crate::session::env_string`] (an empty value
/// counts as unset) and parsed into `T`; an unset, empty, or unparseable value
/// falls through to the config source and finally the default. Every CLI
/// setting resolves through this one chain so precedence lives in a single
/// place. `from_config` is a thunk rather than a `Fn(&Config)` so callers with
/// no config source (e.g. the daemon control address) can pass `|| None`.
pub(crate) fn resolve_setting<T: FromStr>(
    flag: Option<T>,
    env: &str,
    from_config: impl FnOnce() -> Option<T>,
    default: T,
) -> T {
    flag.or_else(|| crate::session::env_string(env).and_then(|value| value.parse().ok()))
        .or_else(from_config)
        .unwrap_or(default)
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
    pub fn set_system_backend(&mut self, backend: ConfiguredBackend) -> Result<()> {
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
        let value = toml::Value::try_from(backend).context("serialize backend as TOML")?;
        system.insert("runtime".to_string(), value);
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let rendered = toml::to_string_pretty(&self.doc).context("serialize config TOML")?;
        omnifs_workspace::io::write_atomic(&self.path, rendered.as_bytes(), 0o644)
            .with_context(|| format!("write {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_round_trips() {
        // Guards the on-disk `[system].runtime` token: `setup` writes it and `up`
        // reads it, so a rename of the serialized form would silently break the
        // runtime selection across a CLI upgrade.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let mut file = ConfigFile::load(&path).unwrap();
        file.set_system_backend(ConfiguredBackend::Native).unwrap();
        file.save().unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("runtime = \"native\""), "got:\n{raw}");

        let config = Config::load(&path).unwrap();
        assert_eq!(config.system.runtime, Some(ConfiguredBackend::Native));
        assert_eq!(config.backend(), ConfiguredBackend::Native);
    }
}
