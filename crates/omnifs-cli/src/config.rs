//! Global `config.toml` loader. Lives at `paths.config_file`.
//!
//! Resolution order is: CLI flag > env var > config file > built-in default.
//! Missing file is not an error; malformed file is. Commands load it from
//! their resolved workspace when they need the optional frontend's image.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub system: ConfigSystem,
    pub telemetry: ConfigTelemetry,
}

/// Local-only dogfood telemetry policy. On by default; `[telemetry] enabled =
/// false` opts out. The CLI honors it for its own `cli.jsonl` writer and
/// propagates it to the daemon it launches (via `OMNIFS_TELEMETRY`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigTelemetry {
    pub enabled: bool,
}

impl Default for ConfigTelemetry {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigSystem {
    /// Override for the optional Docker-hosted FUSE frontend's image. The
    /// daemon itself always runs host-native, so there is no daemon runtime
    /// mode to configure here; this is an opt-in attachment
    /// (`omnifs frontend up`), not a daemon launch policy.
    pub frontend_image: Option<String>,
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

    /// Effective telemetry state for this process: the persistent
    /// `[telemetry] enabled` config field combined with the `OMNIFS_TELEMETRY`
    /// env kill switch, so either one can turn it off.
    pub fn telemetry_enabled(&self) -> bool {
        self.telemetry.enabled && omnifs_workspace::telemetry::enabled_from_env()
    }
}

/// Resolve one setting through the single CLI precedence chain:
/// CLI flag > env var > config file > built-in default.
///
/// The env var is read through [`env_string`] (an empty value
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
    flag.or_else(|| env_string(env).and_then(|value| value.parse().ok()))
        .or_else(from_config)
        .unwrap_or(default)
}

pub(crate) fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_defaults_on_and_parses_off_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        // Absent config: telemetry defaults on.
        let default = Config::load(&path).unwrap();
        assert!(default.telemetry.enabled);

        // Explicit off-switch parses and disables.
        std::fs::write(&path, "[telemetry]\nenabled = false\n").unwrap();
        let off = Config::load(&path).unwrap();
        assert!(!off.telemetry.enabled);

        // A typo'd key is rejected by the strict parser.
        std::fs::write(&path, "[telemetry]\nenabbled = false\n").unwrap();
        assert!(Config::load(&path).is_err());
    }
}
