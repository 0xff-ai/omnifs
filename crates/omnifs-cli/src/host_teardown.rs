//! Host-native teardown for `omnifs down` on macOS.
//!
//! Unmounts the loopback NFS view via `diskutil`, signals the recording daemon
//! so it exits cleanly, and sweeps any orphaned state files. The CLI does not
//! depend on the omnifs-nfs server crate (which links wasmtime through
//! omnifs-host), so the on-disk state shape is mirrored locally here.

use std::path::{Path, PathBuf};
use std::process::Command;
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

/// Tear down every host-native NFS mount recorded under `state_dir`.
///
/// Returns the number of mount records processed. Best-effort and idempotent:
/// an already-unmounted view, a dead daemon, or a failed signal are all
/// non-fatal. A missing `state_dir` means nothing is running (`Ok(0)`).
pub(crate) fn teardown_host_native(state_dir: &Path) -> anyhow::Result<usize> {
    if !state_dir.exists() {
        return Ok(0);
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

    for (state_file, mount_point, pid) in &records {
        unmount(mount_point);
        signal_daemon(*pid);
        settle_state_file(state_file, *pid);
    }

    Ok(records.len())
}

/// `diskutil unmount` is best-effort: an already-unmounted view returns
/// non-zero, which is fine. The output is captured for context only.
fn unmount(mount_point: &Path) {
    match Command::new("diskutil")
        .arg("unmount")
        .arg(mount_point)
        .output()
    {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!(
                "omnifs: diskutil unmount {} reported: {}",
                mount_point.display(),
                stderr.trim()
            );
        },
        Ok(_) => {},
        Err(error) => {
            eprintln!(
                "omnifs: failed to run diskutil unmount {}: {error}",
                mount_point.display()
            );
        },
    }
}

/// Send `SIGTERM` to the recording daemon if it is still alive. A dead pid
/// (failed `kill -0`) needs no signal.
fn signal_daemon(pid: u32) {
    if !pid_alive(pid) {
        return;
    }
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Wait for the daemon to remove its own state file on clean exit. If the file
/// outlives a daemon that is now gone, it is orphaned: remove it ourselves.
fn settle_state_file(state_file: &Path, pid: u32) {
    for _ in 0..10 {
        if !state_file.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    if state_file.exists() && !pid_alive(pid) {
        match std::fs::remove_file(state_file) {
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => {
                eprintln!(
                    "omnifs: failed to remove orphaned mount state {}: {error}",
                    state_file.display()
                );
            },
        }
    }
}
