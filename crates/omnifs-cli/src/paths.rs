//! Path resolution for the omnifs CLI.
//!
//! The canonical layout and resolution logic live in `omnifs_home`; this
//! module re-exports those types and adds the CLI-only two-pass
//! `resolve_with_config` and `overlay_config_paths` that layer
//! `config.toml`'s `[paths]` block on top of the base resolution.

use std::path::PathBuf;

pub use omnifs_home::{PathOverrides, Paths};

/// Two-pass resolution: first pass resolves a no-config `Paths` to find
/// `config_file`, then loads the config and re-resolves with the file's
/// `[paths]` block overlaid as if they were per-purpose env defaults
/// (still beaten by both real env vars and explicit overrides).
pub fn resolve_with_config(
    overrides: PathOverrides,
) -> anyhow::Result<(Paths, crate::config::Config)> {
    let initial = Paths::resolve(overrides.clone());
    let config = crate::config::Config::load(&initial.config_file)?;

    let overrides = overlay_config_paths(overrides, &config.paths);
    let paths = Paths::resolve(overrides);
    Ok((paths, config))
}

/// Overlay `config.toml`'s `[paths]` block as the lowest-priority source,
/// beneath env vars and explicit CLI overrides. The existing `resolve`
/// already reads the per-purpose env vars internally, so we only fill from
/// the config file when neither the CLI override nor the env var is set.
fn overlay_config_paths(
    mut overrides: PathOverrides,
    file: &crate::config::ConfigPaths,
) -> PathOverrides {
    overrides.providers_dir = overrides
        .providers_dir
        .or_else(|| std::env::var_os("OMNIFS_PROVIDERS_DIR").map(PathBuf::from))
        .or_else(|| file.providers_dir.clone());
    overrides
}
