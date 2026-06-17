//! CLI mount enumeration: the single funnel every command uses to list
//! configured mounts.
//!
//! Mounts live as one JSON `Spec` file per mount under the `mounts/`
//! directory. `Workspace::mounts()` is the one path that enumerates them.
//! `Spec`-to-`Resolved` conversion stays in `omnifs_mount::mounts::Catalog`;
//! runtime payload preparation stays in `MountConfig`.

use anyhow::Context as _;
use omnifs_home::Paths;
use std::path::PathBuf;

use crate::session::MountConfig;

/// The CLI workspace: the mount-enumeration funnel over the `mounts/` dir.
#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    paths: Paths,
}

impl Workspace {
    pub(crate) fn new(paths: Paths) -> Self {
        Self { paths }
    }

    /// The single mount-enumeration funnel used by every command.
    ///
    /// Reads one `Spec` per JSON file in the `mounts/` directory and returns
    /// the list sorted by mount name.
    pub(crate) fn mounts(&self) -> anyhow::Result<Vec<MountConfig>> {
        let mut configs = per_file_mount_paths(&self.paths.mounts_dir)?
            .into_iter()
            .map(|path| MountConfig::from_path(&path))
            .collect::<anyhow::Result<Vec<_>>>()?;
        configs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(configs)
    }
}

/// Read the per-file mount spec paths from `mounts_dir`.
///
/// Returns an empty list when the directory does not exist (not an error).
pub(crate) fn per_file_mount_paths(mounts_dir: &std::path::Path) -> anyhow::Result<Vec<PathBuf>> {
    omnifs_mount::mounts::spec_paths_in(mounts_dir)
        .with_context(|| format!("read mount config directory {}", mounts_dir.display()))
}
