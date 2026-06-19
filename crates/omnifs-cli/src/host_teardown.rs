//! Host-native teardown for `omnifs down`.
//!
//! Unmounts the host-native frontend. Linux native uses FUSE directly at the
//! default host mount point. Non-Linux native uses the loopback NFS state files
//! written by the daemon, signals the recording daemon so it exits cleanly, and
//! sweeps any orphaned state files.

#[cfg(not(target_os = "linux"))]
use std::collections::HashSet;
use std::path::Path;
#[cfg(not(target_os = "linux"))]
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(target_os = "linux")]
use anyhow::Context as _;
#[cfg(not(target_os = "linux"))]
use omnifs_nfs::NfsMountState;

/// State-file schema version this CLI understands. A daemon-side bump lands in
/// `TeardownSummary::skipped` so `down` does not claim "nothing is running".
#[cfg(not(target_os = "linux"))]
const STATE_VERSION: u8 = 1;

/// Outcome of a host-native teardown sweep, classified so `omnifs down` reports
/// only what actually happened.
#[derive(Debug, Default, PartialEq, Eq)]
#[cfg(not(target_os = "linux"))]
pub(crate) struct TeardownSummary {
    /// Records whose daemon was alive: a real running mount we tore down.
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

#[cfg(target_os = "linux")]
pub(crate) fn teardown_host_native_fuse(mount_point: &Path) -> anyhow::Result<bool> {
    if !omnifs_nfs::mount_is_active(mount_point) {
        return Ok(false);
    }

    let status = Command::new("fusermount")
        .arg("-u")
        .arg(mount_point)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("run fusermount -u")?;

    anyhow::ensure!(status.success(), "fusermount -u exited with {status}");
    anyhow::ensure!(
        mount_settled(mount_point),
        "{} is still mounted; re-run `omnifs down`",
        mount_point.display()
    );
    Ok(true)
}

/// Tear down every host-native NFS mount recorded under `state_dir`.
///
/// Best-effort and idempotent: an already-unmounted view, a dead daemon, or a
/// failed signal are all non-fatal. A missing `state_dir` means nothing is
/// running (an empty summary).
#[cfg(not(target_os = "linux"))]
pub(crate) fn teardown_host_native_nfs(state_dir: &Path) -> anyhow::Result<TeardownSummary> {
    if !state_dir.exists() {
        return Ok(TeardownSummary::default());
    }

    let mut summary = TeardownSummary::default();
    let mut seen_mount_points = HashSet::new();
    for entry in std::fs::read_dir(state_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let state = match read_state(&path) {
            Ok(state) => state,
            Err(error) => {
                eprintln!("omnifs: skipping mount state {}: {error}", path.display());
                summary.skipped += 1;
                continue;
            },
        };
        if state.version != STATE_VERSION {
            eprintln!(
                "omnifs: skipping mount state {} (unsupported version {})",
                path.display(),
                state.version
            );
            summary.skipped += 1;
            continue;
        }
        if seen_mount_points.insert(state.mount_point.clone()) {
            tear_down_one(&path, &state.mount_point, state.pid, &mut summary);
        }
    }

    Ok(summary)
}

#[cfg(not(target_os = "linux"))]
fn read_state(path: &Path) -> anyhow::Result<NfsMountState> {
    let file = std::fs::File::open(path)?;
    Ok(serde_json::from_reader(file)?)
}

/// Tear down one recorded mount and record the outcome in `summary`.
#[cfg(not(target_os = "linux"))]
fn tear_down_one(state_file: &Path, mount_point: &Path, pid: u32, summary: &mut TeardownSummary) {
    // A live daemon means a real running mount; a dead one means we are only
    // sweeping the stale file it left behind.
    let was_running = pid_alive(pid);
    // A live daemon self-exits once it sees the mount disappear, so a clean
    // unmount suffices. A dead daemon leaves a stale mount whose NFS server is
    // gone, where a clean unmount can hang: force it.
    unmount(mount_point, !was_running);
    if was_running {
        signal_term(pid);
    }

    if !mount_settled(mount_point) {
        // Still mounted: keep the state file so a later `down` retries.
        summary.failed.push(mount_point.to_path_buf());
        return;
    }
    // Mount is gone. The daemon removes its own state file on clean exit;
    // remove it ourselves if it lingered or was orphaned.
    remove_state_file(state_file);
    if was_running {
        summary.unmounted += 1;
    } else {
        summary.swept_orphans += 1;
    }
}

/// Unmount the loopback view. `force` is used when the recording daemon is
/// already dead, so a stale mount whose NFS server has vanished does not hang a
/// clean unmount. Output is swallowed: `diskutil` prints a scary "Unmount
/// failed" even for stale mounts it ultimately clears, so the authoritative
/// signal is whether the mount survives (see `mount_settled`), not its exit text.
#[cfg(target_os = "macos")]
fn unmount(mount_point: &Path, force: bool) {
    let mut command = Command::new("diskutil");
    command.arg("unmount");
    if force {
        command.arg("force");
    }
    let _ = command
        .arg(mount_point)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
fn unmount(mount_point: &Path, force: bool) {
    let mut command = Command::new("umount");
    if force {
        command.arg("-f");
    }
    let _ = command
        .arg(mount_point)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Best-effort SIGTERM so a live daemon exits promptly and releases the control
/// port; a dead pid (the signal lands after the daemon self-exits) is harmless.
#[cfg(not(target_os = "linux"))]
fn signal_term(pid: u32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(target_os = "linux"))]
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Poll until `mount_point` leaves the OS mount table, up to ~6s (a live daemon
/// needs a beat to unmount after SIGTERM). Uses the cross-platform live
/// mount-table check from omnifs-nfs (`/proc/mounts` on Linux, `mount` on macOS).
fn mount_settled(mount_point: &Path) -> bool {
    for attempt in 0..12 {
        if !omnifs_nfs::mount_is_active(mount_point) {
            return true;
        }
        if attempt + 1 < 12 {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    false
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
