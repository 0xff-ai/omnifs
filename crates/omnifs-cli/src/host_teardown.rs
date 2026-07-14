//! Local frontend teardown driven by runner-owned mount state.

use std::path::Path;
use std::time::Duration;

use omnifs_mtab::{MountKind, MountState, Platform, UnmountCommand};

const UNMOUNT_POLL_CADENCE: Duration = Duration::from_millis(500);
const UNMOUNT_POLL_ATTEMPTS: usize = 12;

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
        if !omnifs_nfs::mount_is_active(&state.mount_point) {
            if let Some(error) = remove_state_file(&path) {
                anyhow::bail!(error)
            }
            return Ok(());
        }
        if !local_mount_is_owned(&state) {
            anyhow::bail!(
                "could not unmount {}: mount is not owned by omnifs; refusing to unmount it",
                state.mount_point.display()
            );
        }
        let mount_point = state.mount_point.clone();
        let command = match state.kind {
            MountKind::Nfs { .. } => {
                UnmountCommand::nfs_graceful(Platform::current(), &mount_point)
            },
            MountKind::Fuse => UnmountCommand::graceful(Platform::current(), &mount_point),
        };
        command.run_quiet().map_err(|error| {
            anyhow::anyhow!("could not unmount {}: {error}", mount_point.display())
        })?;
        if !poll_until_unmounted(&mount_point, UNMOUNT_POLL_CADENCE, UNMOUNT_POLL_ATTEMPTS) {
            anyhow::bail!(
                "could not unmount {}: mount remained active after waiting {} seconds",
                mount_point.display(),
                UNMOUNT_POLL_CADENCE
                    .as_secs()
                    .saturating_mul(UNMOUNT_POLL_ATTEMPTS as u64)
            );
        }
        if let Some(error) = remove_state_file(&path) {
            anyhow::bail!(error)
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
