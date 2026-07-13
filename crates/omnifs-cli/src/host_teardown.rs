//! Local frontend teardown driven by runner-owned mount state.

use std::path::{Path, PathBuf};
use std::time::Duration;

use omnifs_mtab::{MountKind, MountState, Platform, UnmountCommand};

const UNMOUNT_POLL_CADENCE: Duration = Duration::from_millis(500);
const UNMOUNT_POLL_ATTEMPTS: usize = 12;

/// A host frontend teardown failure with the reason that made it unsafe or
/// impossible to remove the mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TeardownFailure {
    pub(crate) mount_point: PathBuf,
    pub(crate) reason: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TeardownSummary {
    pub unmounted: usize,
    pub swept_orphans: usize,
    pub failed: Vec<TeardownFailure>,
    pub skipped: usize,
    pub errors: Vec<String>,
}

impl TeardownSummary {
    fn tear_down_one(&mut self, state_file: &Path, state: MountState, force: bool) {
        if !omnifs_nfs::mount_is_active(&state.mount_point) {
            if let Some(error) = remove_state_file(state_file) {
                self.errors.push(error);
            }
            self.swept_orphans += 1;
            return;
        }
        if !local_mount_is_owned(&state) {
            self.failed.push(TeardownFailure {
                mount_point: state.mount_point,
                reason: "mount is not owned by omnifs; refusing to unmount it".to_owned(),
            });
            return;
        }

        let mount_point = state.mount_point.clone();
        let command = match (&state.kind, force) {
            (MountKind::Nfs { .. }, false) => {
                UnmountCommand::nfs_graceful(Platform::current(), &mount_point)
            },
            (_, false) => UnmountCommand::graceful(Platform::current(), &mount_point),
            (_, true) => UnmountCommand::forced(Platform::current(), &mount_point),
        };
        if let Err(error) = command.run_quiet() {
            self.failed.push(TeardownFailure {
                mount_point,
                reason: format!("unmount command failed: {error}"),
            });
            return;
        }

        if !poll_until_unmounted(&mount_point, UNMOUNT_POLL_CADENCE, UNMOUNT_POLL_ATTEMPTS) {
            self.failed.push(TeardownFailure {
                mount_point,
                reason: format!(
                    "mount remained active after waiting {} seconds",
                    UNMOUNT_POLL_CADENCE
                        .as_secs()
                        .saturating_mul(UNMOUNT_POLL_ATTEMPTS as u64)
                ),
            });
            return;
        }
        if let Some(error) = remove_state_file(state_file) {
            self.errors.push(error);
        }
        self.unmounted += 1;
    }
}

pub(crate) fn local_mount_is_owned(state: &MountState) -> bool {
    match &state.kind {
        MountKind::Nfs { .. } => omnifs_nfs::mount_is_omnifs(&state.mount_point),
        MountKind::Fuse => fuse_mount_is_omnifs(&state.mount_point),
    }
}

#[cfg(target_os = "linux")]
fn fuse_mount_is_omnifs(mount_point: &Path) -> bool {
    omnifs_mtab::proc_mounts::find_mount(mount_point)
        .is_some_and(|mount| mount.device == "omnifs" && mount.fs_type.starts_with("fuse"))
}

#[cfg(not(target_os = "linux"))]
fn fuse_mount_is_omnifs(_mount_point: &Path) -> bool {
    false
}

pub(crate) fn teardown_local_frontends(
    state_root: &Path,
    force: bool,
) -> anyhow::Result<TeardownSummary> {
    let mut summary = TeardownSummary::default();
    for path in MountState::files_under(state_root)? {
        match MountState::read_file(&path) {
            Ok(state) => summary.tear_down_one(&path, state, force),
            Err(_error) => {
                summary.skipped += 1;
            },
        }
    }
    Ok(summary)
}

/// Tear down exactly one host frontend location. The location is the identity
/// boundary; no sibling state leaf is touched when a caller disables one of
/// several host frontends.
pub(crate) fn teardown_local_frontend(
    state_root: &Path,
    location: &Path,
    nfs: bool,
) -> anyhow::Result<()> {
    let root = location;
    for path in MountState::files_under(state_root)? {
        let Ok(state) = MountState::read_file(&path) else {
            continue;
        };
        if state.mount_point != root {
            continue;
        }
        let is_nfs = matches!(state.kind, MountKind::Nfs { .. });
        if is_nfs != nfs {
            continue;
        }
        let mut summary = TeardownSummary::default();
        summary.tear_down_one(&path, state, false);
        if let Some(error) = summary.errors.into_iter().next() {
            anyhow::bail!(error)
        }
        if let Some(failure) = summary.failed.into_iter().next() {
            anyhow::bail!(
                "could not unmount {}: {}",
                failure.mount_point.display(),
                failure.reason
            )
        }
        return Ok(());
    }
    Ok(())
}

pub(crate) fn poll_until_unmounted(mount_point: &Path, cadence: Duration, attempts: usize) -> bool {
    for attempt in 0..attempts {
        if !omnifs_nfs::mount_is_active(mount_point) {
            return true;
        }
        if attempt + 1 < attempts {
            std::thread::sleep(cadence);
        }
    }
    false
}

fn remove_state_file(state_file: &Path) -> Option<String> {
    match std::fs::remove_file(state_file) {
        Ok(()) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(format!(
            "failed to remove mount state {}: {error}",
            state_file.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_record_does_not_hide_healthy_sibling() {
        let root = tempfile::tempdir().unwrap();
        let good_dir = root.path().join("nfs/good");
        let good = omnifs_mtab::StateFile::write_nfs(
            &root.path().join("mount"),
            "127.0.0.1:2049".parse().unwrap(),
            &good_dir,
        )
        .unwrap();
        let corrupt_dir = root.path().join("fuse/corrupt");
        std::fs::create_dir_all(&corrupt_dir).unwrap();
        std::fs::write(corrupt_dir.join("mount-corrupt.json"), b"not json").unwrap();

        let summary = teardown_local_frontends(root.path(), false).unwrap();
        assert_eq!(summary.swept_orphans, 1);
        assert_eq!(summary.skipped, 1);
        assert!(!good.path().exists());
    }
}
