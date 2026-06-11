//! CLI workspace: config.toml read view and the single mount-enumeration funnel.
//!
//! `Workspace` owns the host user's `config.toml` (immutable read view and
//! doc-preserving surgical mutators) and provides the one path every command
//! uses to enumerate configured mounts. `Spec`-to-`Resolved` conversion stays
//! in `omnifs_mount::mounts::Catalog`; credential materialization stays in
//! `Session`.
//!
//! The mount funnel merges two sources in order:
//! 1. `[[mounts]]` entries inline in `config.toml`, in declaration order.
//! 2. Per-file specs in the `mounts/` directory, sorted by file stem.

use anyhow::Context as _;
use omnifs_home::Paths;
use omnifs_mount::mounts::Spec;
use std::path::PathBuf;

use crate::config::ConfigFile;
use crate::session::MountConfig;

/// The CLI workspace: config.toml ownership and the mount-enumeration funnel.
#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    paths: Paths,
    /// Inline `[[mounts]]` from `config.toml`, in declaration order.
    inline_mounts: Vec<Spec>,
}

impl Workspace {
    pub(crate) fn new(paths: Paths, inline_mounts: Vec<Spec>) -> Self {
        Self {
            paths,
            inline_mounts,
        }
    }

    /// The single mount-enumeration funnel used by every command.
    ///
    /// Merges inline `config.toml` `[[mounts]]` with per-file specs from the
    /// `mounts/` directory and returns the combined list sorted by mount name.
    pub(crate) fn mounts(&self) -> anyhow::Result<Vec<MountConfig>> {
        let mut configs = Vec::new();

        for spec in &self.inline_mounts {
            configs.push(MountConfig::from_parsed(
                spec.clone(),
                self.paths.config_file.clone(),
            )?);
        }

        let per_file_paths = per_file_mount_paths(&self.paths.mounts_dir)?;
        for path in per_file_paths {
            configs.push(MountConfig::from_path(&path)?);
        }

        configs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(configs)
    }

    /// Write `spec` into `config.toml`, inserting or replacing by mount name.
    ///
    /// Preserves all existing sections and comments.
    pub(crate) fn upsert_mount(&self, spec: &Spec) -> anyhow::Result<()> {
        let mut file = ConfigFile::load(&self.paths.config_file)?;
        file.upsert_mount(spec)?;
        file.save()
    }

    /// Remove the named mount from the inline `[[mounts]]` array in
    /// `config.toml`. Returns `true` if an entry was removed.
    ///
    /// Preserves all existing sections and comments.
    /// Returns the inline `[[mounts]]` specs from `config.toml`.
    pub(crate) fn inline_mounts(&self) -> &[Spec] {
        &self.inline_mounts
    }

    pub(crate) fn remove_inline_mount(&self, name: &str) -> anyhow::Result<bool> {
        let mut file = ConfigFile::load(&self.paths.config_file)?;
        let removed = file.remove_mount(name)?;
        if removed {
            file.save()?;
        }
        Ok(removed)
    }
}

/// Read the per-file mount spec paths from `mounts_dir`.
///
/// Returns an empty list when the directory does not exist (not an error).
pub(crate) fn per_file_mount_paths(mounts_dir: &std::path::Path) -> anyhow::Result<Vec<PathBuf>> {
    omnifs_mount::mounts::spec_paths_in(mounts_dir)
        .with_context(|| format!("read mount config directory {}", mounts_dir.display()))
}
