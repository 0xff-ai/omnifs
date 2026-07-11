//! Filesystem teardown driven by runner-owned mount state.

use std::path::Path;
#[cfg(not(target_os = "linux"))]
use std::path::PathBuf;
use std::time::Duration;

#[cfg(not(target_os = "linux"))]
use omnifs_mtab::NfsMountState;
#[cfg(not(target_os = "linux"))]
use omnifs_mtab::{Platform, UnmountCommand};

/// Outcome of an NFS teardown sweep, classified so `omnifs down` reports
/// only what actually happened.
#[derive(Debug, Default, PartialEq, Eq)]
#[cfg(not(target_os = "linux"))]
pub(crate) struct TeardownSummary {
    /// Live mounts we tore down.
    pub unmounted: usize,
    /// Records left by a dead daemon: only a stale state file to sweep.
    pub swept_orphans: usize,
    /// Mount points still mounted after the unmount attempt. Their state files
    /// are kept so a later `omnifs down` can retry.
    pub failed: Vec<PathBuf>,
    /// State files present but unreadable: a parse error, or a version this CLI
    /// does not understand. A daemon-side format bump lands here, so `down`
    /// must not conclude "nothing is running" while this is > 0.
    pub skipped: usize,
}

#[cfg(not(target_os = "linux"))]
impl TeardownSummary {
    /// Tear down one recorded mount and record the outcome.
    ///
    /// Recorded mounts receive a graceful unmount unless the user requested
    /// `--force`. PID liveness never authorizes teardown.
    fn tear_down_one(&mut self, state_file: &Path, mount_point: &Path, force: bool) {
        if !omnifs_nfs::mount_is_active(mount_point) {
            remove_state_file(state_file);
            self.swept_orphans += 1;
            return;
        }
        if !omnifs_nfs::mount_is_omnifs(mount_point) {
            self.failed.push(mount_point.to_path_buf());
            return;
        }
        unmount(mount_point, force);

        if !mount_settled(mount_point) {
            // Still mounted: keep the state file so a later `down` retries.
            self.failed.push(mount_point.to_path_buf());
            return;
        }
        // Mount is gone. The daemon removes its own state file on clean exit;
        // remove it ourselves if it lingered or was orphaned.
        remove_state_file(state_file);
        self.unmounted += 1;
    }
}

/// Tear down every Omnifs NFS mount recorded under `state_dir`.
///
/// Best-effort and idempotent. A missing `state_dir` means nothing is running
/// (an empty summary). `force` selects the unmount mode only after the live
/// mount table confirms the mount is Omnifs's NFS export.
#[cfg(not(target_os = "linux"))]
pub(crate) fn teardown_host_native_nfs(
    state_dir: &Path,
    force: bool,
) -> anyhow::Result<TeardownSummary> {
    if !state_dir.exists() {
        return Ok(TeardownSummary::default());
    }

    let mut summary = TeardownSummary::default();
    let mut states = Vec::new();
    for entry in std::fs::read_dir(state_dir)? {
        let path = entry?.path();
        if !is_mount_state_file(&path) {
            continue;
        }
        let state = match NfsMountState::read_file(&path) {
            Ok(state) => state,
            Err(error) => {
                eprintln!("omnifs: skipping mount state {}: {error}", path.display());
                summary.skipped += 1;
                continue;
            },
        };
        if state.version != NfsMountState::VERSION {
            eprintln!(
                "omnifs: skipping mount state {} (unsupported version {})",
                path.display(),
                state.version
            );
            summary.skipped += 1;
            continue;
        }
        states.push((path, state));
    }

    for (path, state) in states {
        summary.tear_down_one(&path, &state.mount_point, force);
    }

    Ok(summary)
}

/// Unmount the loopback view. Output is swallowed because the authoritative
/// signal is whether the mount survives (see `mount_settled`).
#[cfg(target_os = "macos")]
fn unmount(mount_point: &Path, force: bool) {
    let command = if force {
        UnmountCommand::forced(Platform::Macos, mount_point)
    } else {
        UnmountCommand::graceful(Platform::Macos, mount_point)
    };
    let _ = command.run_quiet();
}

#[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
fn unmount(mount_point: &Path, force: bool) {
    let command = if force {
        UnmountCommand::forced(Platform::Other, mount_point)
    } else {
        UnmountCommand::graceful(Platform::Other, mount_point)
    };
    let _ = command.run_quiet();
}

#[cfg(not(target_os = "linux"))]
fn is_mount_state_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("mount-")
                && Path::new(name)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        })
}

/// Poll the OS mount table at `cadence` until `mount_point` is no longer active,
/// up to `attempts` checks. Uses the cross-platform live mount-table check from
/// omnifs-nfs (`/proc/mounts` on Linux, `mount` on macOS). Returns true once the
/// mount is gone.
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

/// Poll until `mount_point` leaves the OS mount table, up to ~6s.
fn mount_settled(mount_point: &Path) -> bool {
    poll_until_unmounted(mount_point, Duration::from_millis(500), 12)
}

#[cfg(not(target_os = "linux"))]
fn remove_state_file(state_file: &Path) {
    match std::fs::remove_file(state_file) {
        Ok(()) => {},
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => {
            eprintln!(
                "omnifs: failed to remove mount state {}: {error}",
                state_file.display()
            );
        },
    }
}
