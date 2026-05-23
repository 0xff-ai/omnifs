//! Global `config.toml` loader. Lives at `paths.config_file`.
//!
//! Resolution order is: CLI flag > env var > config file > built-in default.
//! Missing file is not an error; malformed file is. The Config is loaded
//! once and threaded into commands that need it.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub container_name: Option<String>,
    pub image: Option<String>,
    pub paths: ConfigPaths,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigPaths {
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
        config.paths.mounts_dir = config.paths.mounts_dir.map(expand_tilde);
        config.paths.providers_dir = config.paths.providers_dir.map(expand_tilde);
        Ok(config)
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
