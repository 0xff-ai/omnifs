//! Directory resolution for the daemon.
//!
//! Resolution order per directory: explicit CLI flag, per-purpose env var
//! (`OMNIFS_CONFIG_DIR`, `OMNIFS_CACHE_DIR`, `OMNIFS_PROVIDERS_DIR`),
//! `OMNIFS_HOME`, then `~/.omnifs`. This mirrors the CLI's resolution for
//! the subset of directories the daemon consumes; the daemon does not read
//! `config.toml`.

use std::path::PathBuf;

const DEFAULT_HOME_SUBDIR: &str = ".omnifs";
const FALLBACK_ROOT: &str = "/root/.omnifs";

#[derive(Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub providers_dir: PathBuf,
}

#[derive(Debug, Default)]
pub struct Overrides {
    pub config_dir: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
}

impl Paths {
    pub fn resolve(overrides: Overrides) -> Self {
        let omnifs_home = std::env::var_os("OMNIFS_HOME").map(PathBuf::from);
        let default_root = default_omnifs_root();

        let config_dir = overrides
            .config_dir
            .or_else(|| std::env::var_os("OMNIFS_CONFIG_DIR").map(PathBuf::from))
            .or_else(|| omnifs_home.clone())
            .unwrap_or_else(|| default_root.clone());

        let cache_dir = overrides
            .cache_dir
            .or_else(|| std::env::var_os("OMNIFS_CACHE_DIR").map(PathBuf::from))
            .or_else(|| omnifs_home.as_ref().map(|h| h.join("cache")))
            .unwrap_or_else(|| default_root.join("cache"));

        let data_dir = omnifs_home
            .as_ref()
            .map_or_else(|| default_root.join("data"), |h| h.join("data"));
        let providers_dir = std::env::var_os("OMNIFS_PROVIDERS_DIR")
            .map_or_else(|| data_dir.join("providers"), PathBuf::from);

        Self {
            config_dir,
            cache_dir,
            providers_dir,
        }
    }
}

fn default_omnifs_root() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(FALLBACK_ROOT),
        |home| PathBuf::from(home).join(DEFAULT_HOME_SUBDIR),
    )
}
