//! Host-native teardown for `omnifs down` on macOS.
//!
//! Unmounts the loopback NFS view via `diskutil`, signals the recording daemon
//! so it exits cleanly, and sweeps any orphaned state files. The CLI does not
//! depend on the omnifs-nfs server crate (which links wasmtime through
//! omnifs-host), so the on-disk state shape is mirrored locally here.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;

/// Mirrors `omnifs_nfs::NfsMountState` on-disk shape (version 1); kept CLI-local
/// so the CLI stays free of the omnifs-nfs server crate, which links wasmtime.
/// Keep in sync.
#[derive(Debug, Deserialize)]
struct NfsMountState {
    version: u8,
    mount_point: PathBuf,
    pid: u32,
}

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

    let mut records: Vec<(PathBuf, PathBuf, u32)> = Vec::new();
    let mut seen_mount_points: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(state_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                eprintln!("omnifs: skipping mount state {}: {error}", path.display());
                continue;
            },
        };
        let state: NfsMountState = match serde_json::from_reader(file) {
            Ok(state) => state,
            Err(error) => {
                eprintln!("omnifs: skipping mount state {}: {error}", path.display());
                continue;
            },
        };
        if state.version != STATE_VERSION {
            eprintln!(
                "omnifs: skipping mount state {} (unsupported version {})",
                path.display(),
                state.version
            );
            continue;
        }
        if seen_mount_points.contains(&state.mount_point) {
            continue;
        }
        seen_mount_points.push(state.mount_point.clone());
        records.push((path, state.mount_point, state.pid));
    }

    let mut summary = TeardownSummary::default();
    for (state_file, mount_point, pid) in &records {
        // A live daemon means a real running mount; a dead one means we are only
        // sweeping the stale file it left behind.
        let was_running = pid_alive(*pid);
        // A live daemon self-exits once it sees the mount disappear, so a clean
        // unmount suffices. A dead daemon leaves a stale mount whose NFS server
        // is gone, where a clean unmount can hang: force it.
        unmount(mount_point, !was_running);
        if was_running {
            signal_term(*pid);
        }

        if mount_settled(mount_point) {
            // Mount is gone. The daemon removes its own state file on clean
            // exit; remove it ourselves if it lingered or was orphaned.
            remove_state_file(state_file);
            if was_running {
                summary.unmounted += 1;
            } else {
                summary.swept_orphans += 1;
            }
        } else {
            // Still mounted: keep the state file so a later `down` retries.
            summary.failed.push(mount_point.clone());
        }
    }

    Ok(summary)
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
    for _ in 0..12 {
        if !mount_present(mount_point) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    !mount_present(mount_point)
}

/// Whether `mount_point` is currently in the OS mount table. Matches the
/// `mount(8)` "<src> on <path> (" form against the canonical path so a sibling
/// mount sharing a path prefix cannot be mistaken for this one.
fn mount_present(mount_point: &Path) -> bool {
    let Ok(output) = Command::new("/sbin/mount").output() else {
        return false;
    };
    let canonical = std::fs::canonicalize(mount_point).map_or_else(
        |_| mount_point.to_string_lossy().into_owned(),
        |path| path.to_string_lossy().into_owned(),
    );
    let needle = format!(" on {canonical} (");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.contains(&needle))
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
