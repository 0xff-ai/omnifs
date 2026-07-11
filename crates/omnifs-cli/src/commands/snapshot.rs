#![allow(clippy::disallowed_macros)] // migrates in wave 2 (cli-redesign)
//! `omnifs snapshot <mount> --out <dir>` — export canonical bytes for audit.

use anyhow::{Context as _, bail};
use clap::Args;
use omnifs_engine::snapshot::MountSnapshot;
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::mounts::Name as MountName;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use crate::workspace::Workspace;

#[derive(Args, Debug, Clone)]
pub struct SnapshotArgs {
    /// Mount name to snapshot.
    pub mount: String,
    /// Empty or absent output directory for the snapshot tree.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
}

impl SnapshotArgs {
    pub async fn run(self) -> anyhow::Result<()> {
        let workspace = Workspace::resolve()?;
        let mount = MountName::new(self.mount.clone())
            .with_context(|| format!("invalid mount name `{}` for snapshot export", self.mount))?;
        require_configured_mount(&workspace, &mount)?;

        let source = if let Some(tar) = workspace
            .daemon()
            .export_mount_if_running(mount.as_str())
            .await?
        {
            unpack_snapshot_tar(&tar, &self.out)?;
            "daemon"
        } else {
            MountSnapshot::from_cache_dir(&workspace.layout().cache_dir, mount.as_str())?
                .write_directory(&self.out)?;
            "cache"
        };

        anstream::eprintln!(
            "Wrote `{}` snapshot to {} ({source})",
            mount,
            WorkspaceLayout::display(&self.out)
        );
        Ok(())
    }
}

fn require_configured_mount(workspace: &Workspace, name: &MountName) -> anyhow::Result<()> {
    let mounts = workspace.mounts()?;
    if mounts.iter().any(|mount| &mount.name == name) {
        return Ok(());
    }
    bail!("no mount config named `{name}`");
}

fn unpack_snapshot_tar(bytes: &[u8], out: &Path) -> anyhow::Result<()> {
    prepare_output_dir(out)?;
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    archive
        .unpack(out)
        .with_context(|| format!("unpack snapshot tar into {}", out.display()))?;
    Ok(())
}

fn prepare_output_dir(out: &Path) -> anyhow::Result<()> {
    if out.exists() {
        if !out.is_dir() {
            bail!("snapshot output {} is not a directory", out.display());
        }
        let has_entries = std::fs::read_dir(out)
            .with_context(|| format!("read snapshot output directory {}", out.display()))?
            .next()
            .transpose()?
            .is_some();
        if has_entries {
            bail!(
                "snapshot output directory {} must be empty or absent",
                out.display()
            );
        }
    } else {
        std::fs::create_dir_all(out)
            .with_context(|| format!("create snapshot output directory {}", out.display()))?;
    }
    Ok(())
}
