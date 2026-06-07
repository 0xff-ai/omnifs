//! Path resolution for the omnifs CLI.
//!
//! Resolution order (per directory):
//!   1. Explicit override from `PathOverrides` (CLI flag)
//!   2. Per-purpose env var (`OMNIFS_CONFIG_DIR`, `OMNIFS_CACHE_DIR`,
//!      `OMNIFS_MOUNTS_DIR`, `OMNIFS_PROVIDERS_DIR`)
//!   3. `OMNIFS_HOME` (fans out: `$OMNIFS_HOME/{config,data,cache}` as the three roots)
//!   4. Default: `$HOME/.omnifs/{config,data,cache}`
//!
//! The default layout is identical on macOS and Linux. Users who want
//! platform-native (XDG, Apple's Library tree) can point the per-purpose
//! env vars at those locations.

use omnifs_core::MountName;
use std::path::{Path, PathBuf};

const DEFAULT_HOME_SUBDIR: &str = ".omnifs";
const FALLBACK_ROOT: &str = "/root/.omnifs";

#[derive(Debug, Clone, Default)]
pub struct PathOverrides {
    pub config_dir: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub mounts_dir: Option<PathBuf>,
    pub providers_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// Directory holding mount instance configs (one JSON per mount).
    /// `omnifs init` writes here.
    pub mounts_dir: PathBuf,
    /// Directory holding compiled provider WASM components, looked up
    /// by the `provider:` field of each mount config.
    pub providers_dir: PathBuf,
    pub credentials_file: PathBuf,
    pub config_file: PathBuf,
}

impl Paths {
    /// Resolve paths from overrides, env, `OMNIFS_HOME`, then the
    /// `$HOME/.omnifs/{config,data,cache}` default.
    pub fn resolve(overrides: PathOverrides) -> Self {
        let omnifs_home = std::env::var_os("OMNIFS_HOME").map(PathBuf::from);
        let default_root = default_omnifs_root();

        let config_dir = overrides
            .config_dir
            .or_else(|| std::env::var_os("OMNIFS_CONFIG_DIR").map(PathBuf::from))
            .or_else(|| omnifs_home.as_ref().map(|h| h.join("config")))
            .unwrap_or_else(|| default_root.join("config"));

        let data_dir = overrides
            .data_dir
            .or_else(|| omnifs_home.as_ref().map(|h| h.join("data")))
            .unwrap_or_else(|| default_root.join("data"));

        let cache_dir = overrides
            .cache_dir
            .or_else(|| std::env::var_os("OMNIFS_CACHE_DIR").map(PathBuf::from))
            .or_else(|| omnifs_home.as_ref().map(|h| h.join("cache")))
            .unwrap_or_else(|| default_root.join("cache"));

        let mounts_dir = overrides
            .mounts_dir
            .or_else(|| std::env::var_os("OMNIFS_MOUNTS_DIR").map(PathBuf::from))
            .unwrap_or_else(|| config_dir.join("mounts"));

        let providers_dir = overrides
            .providers_dir
            .or_else(|| std::env::var_os("OMNIFS_PROVIDERS_DIR").map(PathBuf::from))
            .unwrap_or_else(|| data_dir.join("providers"));

        let credentials_file = config_dir.join("credentials.json");
        let config_file = config_dir.join("config.toml");

        Paths {
            config_dir,
            data_dir,
            cache_dir,
            mounts_dir,
            providers_dir,
            credentials_file,
            config_file,
        }
    }

    /// Two-pass resolution: first pass resolves a no-config `Paths` to find
    /// `config_file`, then loads the config and re-resolves with the file's
    /// `[paths]` block overlaid as if they were per-purpose env defaults
    /// (still beaten by both real env vars and explicit overrides).
    pub fn resolve_with_config(
        overrides: PathOverrides,
    ) -> anyhow::Result<(Paths, crate::config::Config)> {
        let initial = Self::resolve(overrides.clone());
        let config = crate::config::Config::load(&initial.config_file)?;

        let overrides = overlay_config_paths(overrides, &config.paths);
        let paths = Self::resolve(overrides);
        Ok((paths, config))
    }

    /// Home-relativize a path for display (`~/.omnifs/config/mounts`).
    /// Falls back to the full path if HOME is unset or stripping fails.
    pub fn display(path: &Path) -> String {
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            if let Ok(stripped) = path.strip_prefix(&home) {
                return format!("~/{}", stripped.display());
            }
        }
        path.display().to_string()
    }

    pub fn mount_config_path(&self, name: &MountName) -> PathBuf {
        mount_config_path_for(&self.mounts_dir, name)
    }

    pub fn provider_path(&self, provider: &str) -> PathBuf {
        provider_path_for(&self.providers_dir, provider)
    }
}

pub(crate) fn mount_config_path_for(mounts_dir: &Path, name: &MountName) -> PathBuf {
    mounts_dir.join(format!("{name}.json"))
}

pub(crate) fn provider_path_for(providers_dir: &Path, provider: &str) -> PathBuf {
    let provider = PathBuf::from(provider);
    if provider.is_absolute() {
        provider
    } else {
        providers_dir.join(provider)
    }
}

fn default_omnifs_root() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(FALLBACK_ROOT),
        |home| PathBuf::from(home).join(DEFAULT_HOME_SUBDIR),
    )
}

/// Overlay `config.toml`'s `[paths]` block as the lowest-priority source,
/// beneath env vars and explicit CLI overrides. The existing `resolve`
/// already reads the per-purpose env vars internally, so we only fill from
/// the config file when neither the CLI override nor the env var is set.
fn overlay_config_paths(
    mut overrides: PathOverrides,
    file: &crate::config::ConfigPaths,
) -> PathOverrides {
    overrides.mounts_dir = overrides
        .mounts_dir
        .or_else(|| std::env::var_os("OMNIFS_MOUNTS_DIR").map(PathBuf::from))
        .or_else(|| file.mounts_dir.clone());
    overrides.providers_dir = overrides
        .providers_dir
        .or_else(|| std::env::var_os("OMNIFS_PROVIDERS_DIR").map(PathBuf::from))
        .or_else(|| file.providers_dir.clone());
    overrides
}
