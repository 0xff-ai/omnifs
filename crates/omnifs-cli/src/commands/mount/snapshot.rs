//! `omnifs mount snapshot <mount> --out <dir>` — export canonical bytes for audit.

use anyhow::{Context as _, bail};
use clap::Args;
use omnifs_engine::snapshot::{MountSnapshot, WriteProgress};
use omnifs_workspace::layout::WorkspaceLayout;
use omnifs_workspace::mounts::Name as MountName;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use crate::ui::LiveRow;
use crate::workspace::Workspace;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ExportProgress {
    files_written: u64,
    total_files: u64,
    bytes_written: u64,
    total_bytes: u64,
}

struct SnapshotTar<'a> {
    bytes: &'a [u8],
}

impl From<WriteProgress> for ExportProgress {
    fn from(progress: WriteProgress) -> Self {
        Self {
            files_written: progress.files_written,
            total_files: progress.total_files,
            bytes_written: progress.bytes_written,
            total_bytes: progress.total_bytes,
        }
    }
}

impl ExportProgress {
    fn update(self, row: &mut LiveRow) {
        row.update_files_bytes(
            self.files_written,
            self.total_files,
            self.bytes_written,
            self.total_bytes,
        );
    }
}

impl<'a> SnapshotTar<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn unpack(self, out: &Path, row: &mut LiveRow) -> anyhow::Result<ExportProgress> {
        prepare_output_dir(out)?;

        let totals = self.totals()?;
        let mut archive = tar::Archive::new(Cursor::new(self.bytes));
        let mut progress = ExportProgress {
            total_files: totals.total_files,
            total_bytes: totals.total_bytes,
            ..ExportProgress::default()
        };
        for entry in archive.entries().context("read snapshot tar entries")? {
            let mut entry = entry.context("read snapshot tar entry")?;
            let path = entry
                .path()
                .context("read snapshot tar entry path")?
                .into_owned();
            let is_file = entry.header().entry_type().is_file();
            let size = entry.size();
            let unpacked = entry
                .unpack_in(out)
                .with_context(|| format!("unpack snapshot tar into {}", out.display()))?;
            if !unpacked {
                bail!(
                    "refusing to unpack snapshot tar entry `{}`: unsafe path",
                    path.display()
                );
            }
            if is_file {
                progress.files_written += 1;
                progress.bytes_written += size;
                progress.update(row);
            }
        }
        Ok(progress)
    }

    fn totals(&self) -> anyhow::Result<ExportProgress> {
        let mut archive = tar::Archive::new(Cursor::new(self.bytes));
        let mut totals = ExportProgress::default();
        for entry in archive.entries().context("read snapshot tar entries")? {
            let entry = entry.context("read snapshot tar entry")?;
            if entry.header().entry_type().is_file() {
                totals.total_files += 1;
                totals.total_bytes = totals
                    .total_bytes
                    .checked_add(entry.size())
                    .context("snapshot tar byte count overflow")?;
            }
        }
        Ok(totals)
    }
}

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

        let mut row = LiveRow::start("snapshot", "preparing");
        row.update("preparing");
        let result: anyhow::Result<(&str, ExportProgress)> = async {
            if let Some(tar) = workspace
                .daemon()
                .export_mount_if_running(mount.as_str())
                .await?
            {
                Ok((
                    "daemon",
                    SnapshotTar::new(&tar).unpack(&self.out, &mut row)?,
                ))
            } else {
                let snapshot =
                    MountSnapshot::from_cache_dir(&workspace.layout().cache_dir, mount.as_str())?;
                let mut progress = ExportProgress::default();
                snapshot.write_directory_with_progress(&self.out, |update| {
                    progress = update.into();
                    progress.update(&mut row);
                })?;
                Ok(("cache", progress))
            }
        }
        .await;
        let (source, progress) = match result {
            Ok(result) => result,
            Err(error) => {
                row.settle_fail("snapshot failed");
                return Err(error);
            },
        };

        row.settle_ok(format!(
            "`{}` to {} ({source}; {} files, {})",
            mount,
            WorkspaceLayout::display(&self.out),
            progress.files_written,
            LiveRow::human_bytes(progress.bytes_written),
        ));
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

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    fn fixture_tar() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut bytes);
            for (path, contents) in [
                ("one.txt", b"one".as_slice()),
                ("nested/two.txt", b"second".as_slice()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                archive.append_data(&mut header, path, contents).unwrap();
            }
            archive.finish().unwrap();
        }
        bytes
    }

    fn unsafe_fixture_tar() -> Vec<u8> {
        // `tar::Builder` rejects traversal paths up front, so mutate a valid
        // fixture header and recalculate its checksum to exercise the reader's
        // path-safety branch.
        let mut bytes = fixture_tar();
        let path = b"../outside.txt";
        bytes[..100].fill(0);
        bytes[..path.len()].copy_from_slice(path);
        bytes[148..156].fill(b' ');
        let checksum: u32 = bytes[..512].iter().map(|byte| u32::from(*byte)).sum();
        let checksum = format!("{checksum:06o}\0 ");
        bytes[148..156].copy_from_slice(checksum.as_bytes());
        bytes
    }

    #[test]
    fn tar_totals_count_regular_files_and_bytes() {
        let bytes = fixture_tar();
        let totals = SnapshotTar::new(&bytes).totals().unwrap();
        assert_eq!(totals.total_files, 2);
        assert_eq!(totals.total_bytes, 9);
    }

    #[test]
    fn unpack_reports_the_complete_export() {
        let temp = tempfile::tempdir().unwrap();
        let mut row = LiveRow::start("snapshot", "preparing");
        let progress = SnapshotTar::new(&fixture_tar())
            .unpack(temp.path(), &mut row)
            .unwrap();

        assert_eq!(progress.files_written, 2);
        assert_eq!(progress.files_written, progress.total_files);
        assert_eq!(progress.bytes_written, progress.total_bytes);
        assert_eq!(std::fs::read(temp.path().join("one.txt")).unwrap(), b"one");
        assert_eq!(
            std::fs::read(temp.path().join("nested/two.txt")).unwrap(),
            b"second"
        );
    }

    #[test]
    fn output_directory_must_be_empty() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::File::create(temp.path().join("existing"))
            .unwrap()
            .write_all(b"kept")
            .unwrap();
        let mut row = LiveRow::start("snapshot", "preparing");

        let error = SnapshotTar::new(&fixture_tar())
            .unpack(temp.path(), &mut row)
            .unwrap_err();

        assert!(error.to_string().contains("must be empty or absent"));
        assert_eq!(
            std::fs::read(temp.path().join("existing")).unwrap(),
            b"kept"
        );
    }

    #[test]
    fn unpack_rejects_unsafe_entries_instead_of_counting_them() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out");
        let mut row = LiveRow::start("snapshot", "preparing");

        let error = SnapshotTar::new(&unsafe_fixture_tar())
            .unpack(&out, &mut row)
            .unwrap_err();

        assert!(error.to_string().contains("unsafe path"));
        assert!(error.to_string().contains("../outside.txt"));
        assert_eq!(std::fs::read_dir(out).unwrap().count(), 0);
    }
}
