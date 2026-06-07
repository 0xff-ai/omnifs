//! Persisted daemon runtime paths for status collection.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RuntimeState {
    pub mount_point: PathBuf,
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub mounts_dir: PathBuf,
}

impl RuntimeState {
    pub(crate) fn file_path(config_dir: &Path) -> PathBuf {
        config_dir.join("runtime_state.json")
    }

    pub(crate) fn write(&self) -> anyhow::Result<()> {
        fs::create_dir_all(&self.config_dir)?;
        let path = Self::file_path(&self.config_dir);
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, format!("{json}\n"))?;
        Ok(())
    }

    pub(crate) fn load(config_dir: &Path) -> Option<Self> {
        let path = Self::file_path(config_dir);
        let raw = fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }
}
