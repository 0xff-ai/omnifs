//! Path resolution for the omnifs CLI.
//!
//! The canonical layout and resolution logic live in `omnifs_home`; this
//! module re-exports those types and adds the CLI-only two-pass
//! `resolve_with_config` that locates and loads `config.toml`.

pub use omnifs_home::{PathOverrides, Paths, ResolveError};

/// Default host-visible mount point for native launches.
///
/// This is deliberately outside `OMNIFS_HOME`: config/cache state belongs under
/// the omnifs home, while the mounted filesystem should live at a normal
/// user-owned location that does not require creating `/mnt` or `/Volumes`.
pub fn default_host_mount_point() -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("cannot resolve host mount point: set HOME"))?;
    Ok(std::path::PathBuf::from(home).join("omnifs"))
}

/// Two-pass resolution: first pass resolves a no-config `Paths` to find
/// `config_file`, then loads the config.
pub fn resolve_with_config(
    overrides: PathOverrides,
) -> anyhow::Result<(Paths, crate::config::Config)> {
    let initial = Paths::resolve(overrides.clone())?;
    let config = crate::config::Config::load(&initial.config_file)?;

    let paths = Paths::resolve(overrides)?;
    Ok((paths, config))
}
