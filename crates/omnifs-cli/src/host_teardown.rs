//! Local frontend teardown driven by runner-owned mount state.

use std::path::{Path, PathBuf};
use std::time::Duration;

use omnifs_mtab::{MountKind, MountState, Platform, UnmountCommand};

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TeardownSummary {
    pub unmounted: usize,
    pub swept_orphans: usize,
    pub failed: Vec<PathBuf>,
    pub skipped: usize,
}

impl TeardownSummary {
    fn tear_down_one(&mut self, state_file: &Path, state: MountState, force: bool) {
        if !omnifs_nfs::mount_is_active(&state.mount_point) {
            remove_state_file(state_file);
            self.swept_orphans += 1;
            return;
        }
        if !local_mount_is_owned(&state) {
            self.failed.push(state.mount_point);
            return;
        }

        let command = match (&state.kind, force) {
            (MountKind::Nfs { .. }, false) => {
                UnmountCommand::nfs_graceful(Platform::current(), &state.mount_point)
            },
            (_, false) => UnmountCommand::graceful(Platform::current(), &state.mount_point),
            (_, true) => UnmountCommand::forced(Platform::current(), &state.mount_point),
        };
        let _ = command.run_quiet();

        if !poll_until_unmounted(&state.mount_point, Duration::from_millis(500), 12) {
            self.failed.push(state.mount_point);
            return;
        }
        remove_state_file(state_file);
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
    state_dir: &Path,
    force: bool,
) -> anyhow::Result<TeardownSummary> {
    let entries = match std::fs::read_dir(state_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TeardownSummary::default());
        },
        Err(error) => return Err(error.into()),
    };
    let mut paths = entries
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| MountState::is_file(path))
        .collect::<Vec<_>>();
    paths.sort();

    let mut summary = TeardownSummary::default();
    for path in paths {
        match MountState::read_file(&path) {
            Ok(state) => summary.tear_down_one(&path, state, force),
            Err(error) => {
                anstream::eprintln!(
                    "⚠  Skipping local frontend record {}: {error}",
                    path.display()
                );
                summary.skipped += 1;
            },
        }
    }
    Ok(summary)
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

fn remove_state_file(state_file: &Path) {
    match std::fs::remove_file(state_file) {
        Ok(()) => {},
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => eprintln!(
            "omnifs: failed to remove mount state {}: {error}",
            state_file.display()
        ),
    }
}
