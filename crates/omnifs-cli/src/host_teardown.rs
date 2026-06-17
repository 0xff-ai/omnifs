//! Host-native teardown for `omnifs down`.
//!
//! Unmounts the loopback NFS view via `diskutil`, signals the recording daemon
//! so it exits cleanly, and sweeps any orphaned state files. The on-disk state
//! shape is `omnifs_nfs::NfsMountState`, read directly now that the single
//! binary links the nfs crate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use omnifs_nfs::NfsMountState;

/// State-file schema version this CLI understands. A daemon-side bump lands in
/// `TeardownSummary::skipped` so `down` does not claim "nothing is running".
const STATE_VERSION: u8 = 1;

/// Outcome of a host-native teardown sweep, classified so `omnifs down` reports
/// only what actually happened.
#[derive(Debug, Default, PartialEq, Eq)]
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

/// Tear down every host-native NFS mount recorded under `state_dir`.
///
/// Best-effort and idempotent: an already-unmounted view, a dead daemon, or a
/// failed signal are all non-fatal. A missing `state_dir` means nothing is
/// running (an empty summary).
pub(crate) fn teardown_host_native(state_dir: &Path) -> anyhow::Result<TeardownSummary> {
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

fn read_state(path: &Path) -> anyhow::Result<NfsMountState> {
    let file = std::fs::File::open(path)?;
    Ok(serde_json::from_reader(file)?)
}

/// Tear down one recorded mount and record the outcome in `summary`.
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

/// Best-effort SIGTERM so a live daemon exits promptly and releases the control
/// port; a dead pid (the signal lands after the daemon self-exits) is harmless.
fn signal_term(pid: u32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

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
/// needs a beat to unmount after SIGTERM). Returns true once it is gone.
fn mount_settled(mount_point: &Path) -> bool {
    let needle = mount_table_needle(mount_point);
    for attempt in 0..12 {
        if !mount_present(&needle) {
            return true;
        }
        if attempt + 1 < 12 {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    false
}

/// The `mount(8)` "<src> on <path> (" fragment for `mount_point`, matched
/// against the canonical path so a sibling mount sharing a path prefix cannot be
/// mistaken for this one. Computed once per teardown, reused across poll ticks.
fn mount_table_needle(mount_point: &Path) -> String {
    let canonical = std::fs::canonicalize(mount_point).map_or_else(
        |_| mount_point.to_string_lossy().into_owned(),
        |path| path.to_string_lossy().into_owned(),
    );
    format!(" on {canonical} (")
}

fn mount_present(needle: &str) -> bool {
    let Ok(output) = Command::new("/sbin/mount").output() else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.contains(needle))
}

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
