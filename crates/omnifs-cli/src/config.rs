//! Global `config.toml` loader. Lives at `paths.config_file`.
//!
//! Resolution order is: CLI flag > env var > config file > built-in default.
//! Missing file is not an error; malformed file is. The Config is loaded
//! once and threaded into commands that need it.

use anyhow::{Context, Result};
use omnifs_mount_schema::mounts::Spec;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub system: ConfigSystem,
    /// Legacy top-level setting, still accepted for existing config files.
    pub container_name: Option<String>,
    /// Legacy top-level setting, still accepted for existing config files.
    pub image: Option<String>,
    pub paths: ConfigPaths,
    pub mounts: Vec<Spec>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigSystem {
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub providers_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigPaths {
    /// Legacy per-mount JSON directory.
    pub mounts_dir: Option<PathBuf>,
    /// Compiled provider WASM components directory.
    pub providers_dir: Option<PathBuf>,
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
        config.paths.mounts_dir = config.paths.mounts_dir.map(expand_tilde);
        config.paths.providers_dir = config.paths.providers_dir.map(expand_tilde);
        Ok(config)
    }

    fn apply_system_section(&mut self) {
        self.container_name = self
            .container_name
            .clone()
            .or(self.system.container_name.clone());
        self.image = self.image.clone().or(self.system.image.clone());
        self.paths.providers_dir = self
            .paths
            .providers_dir
            .clone()
            .or(self.system.providers_dir.clone());
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

    pub fn upsert_mount(&mut self, spec: &Spec) -> Result<()> {
        let root = self
            .doc
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("{} is not a TOML table", self.path.display()))?;
        let mounts = root
            .entry("mounts".to_string())
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        let mounts = mounts.as_array_mut().ok_or_else(|| {
            anyhow::anyhow!("{} has a non-array [[mounts]] value", self.path.display())
        })?;
        let value = toml::Value::try_from(spec).context("serialize mount as TOML")?;
        if let Some(existing) = mounts
            .iter_mut()
            .find(|mount| mount.get("mount").and_then(toml::Value::as_str) == Some(&spec.mount))
        {
            *existing = value;
        } else {
            mounts.push(value);
        }
        Ok(())
    }

    pub fn remove_mount(&mut self, name: &str) -> Result<bool> {
        let Some(root) = self.doc.as_table_mut() else {
            anyhow::bail!("{} is not a TOML table", self.path.display());
        };
        let Some(mounts) = root.get_mut("mounts").and_then(toml::Value::as_array_mut) else {
            return Ok(false);
        };
        let before = mounts.len();
        mounts.retain(|mount| mount.get("mount").and_then(toml::Value::as_str) != Some(name));
        Ok(mounts.len() != before)
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

fn expand_tilde(p: PathBuf) -> PathBuf {
    if let Ok(stripped) = p.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(stripped);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_inline_mounts() {
        let config: Config = toml::from_str(
            r#"
                [[mounts]]
                provider = "omnifs_provider_dns.wasm"
                mount = "dns"

                [mounts.config]
                resolver = "1.1.1.1"
            "#,
        )
        .unwrap();

        assert_eq!(config.mounts.len(), 1);
        assert_eq!(config.mounts[0].mount, "dns");
        assert_eq!(
            config.mounts[0].config_raw.as_ref().unwrap().as_value()["resolver"],
            "1.1.1.1"
        );
    }

    #[test]
    fn system_section_feeds_legacy_runtime_fields() {
        let config: Config = toml::from_str(
            r#"
                [system]
                container_name = "omnifs-test"
                image = "ghcr.io/example/omnifs:test"
                providers_dir = "~/providers"
            "#,
        )
        .unwrap();
        let mut config = config;
        config.apply_system_section();

        assert_eq!(config.container_name.as_deref(), Some("omnifs-test"));
        assert_eq!(config.image.as_deref(), Some("ghcr.io/example/omnifs:test"));
        assert_eq!(
            config.paths.providers_dir,
            Some(PathBuf::from("~/providers"))
        );
    }
}
