//! Host-native teardown for `omnifs down`.
//!
//! Unmounts the host-native frontend. Linux native uses FUSE directly at the
//! default host mount point. Non-Linux native uses the loopback NFS state files
//! written by the daemon, signals the recording daemon so it exits cleanly, and
//! sweeps any orphaned state files.

#[cfg(not(target_os = "linux"))]
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
#[cfg(not(target_os = "linux"))]
use std::path::PathBuf;
use std::process::Command;
#[cfg(not(target_os = "linux"))]
use std::process::Stdio;
use std::time::Duration;

#[cfg(target_os = "linux")]
use anyhow::Context as _;
#[cfg(not(target_os = "linux"))]
use omnifs_mtab::NfsMountState;
use omnifs_mtab::{Platform, UnmountCommand};

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

pub(crate) fn open_handle_summary(mount_point: &Path) -> Option<String> {
    let output = Command::new("lsof")
        .args(["-F", "pcfn", "--"])
        .arg(mount_point)
        .output();
    match output {
        Ok(output) if output.status.success() || !output.stdout.is_empty() => {
            render_lsof_handles(&String::from_utf8_lossy(&output.stdout))
        },
        Ok(_) => None,
        Err(error) => Some(format!(
            "Could not inspect open mount handles with `lsof`: {error}"
        )),
    }
}

fn render_lsof_handles(fields: &str) -> Option<String> {
    let mut processes: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut current = None;
    let mut current_fd = None::<String>;

    for line in fields.lines() {
        if line.is_empty() {
            continue;
        }
        let (kind, value) = line.split_at(1);
        match kind {
            "p" => {
                processes.push((value.to_string(), "unknown".to_string(), Vec::new()));
                current = Some(processes.len() - 1);
                current_fd = None;
            },
            "c" => {
                if let Some(index) = current {
                    processes[index].1 = value.to_string();
                }
            },
            "f" => {
                current_fd = Some(value.to_string());
            },
            "n" => {
                if let Some(index) = current {
                    processes[index].2.push(format!(
                        "{} {}",
                        current_fd.as_deref().unwrap_or("?"),
                        value
                    ));
                }
            },
            _ => {},
        }
    }

    if processes.is_empty() {
        return None;
    }

    let mut out = String::from("Open handles inside the mount:\n");
    for (pid, command, handles) in processes {
        let _ = write!(out, "  {command} pid {pid}");
        if !handles.is_empty() {
            out.push_str(": ");
            out.push_str(&handles.join("; "));
        }
        out.push('\n');
    }
    Some(out.trim_end().to_string())
}

#[cfg(target_os = "linux")]
pub(crate) fn teardown_host_native_fuse(mount_point: &Path) -> anyhow::Result<bool> {
    if !omnifs_nfs::mount_is_active(mount_point) {
        return Ok(false);
    }

    UnmountCommand::graceful(Platform::Linux, mount_point)
        .run_quiet()
        .context("run fusermount -u")?;
    anyhow::ensure!(
        mount_settled(mount_point),
        "{} is still mounted; re-run `omnifs down`",
        mount_point.display()
    );
    Ok(true)
}

#[cfg(target_os = "linux")]
pub(crate) fn force_unmount_host_native(mount_point: &Path) {
    let _ = UnmountCommand::forced(Platform::Linux, mount_point).run_quiet();
}

/// Tear down every host-native NFS mount recorded under `state_dir`.
///
/// Best-effort and idempotent: an already-unmounted view, a dead daemon, or a
/// failed signal are all non-fatal. A missing `state_dir` means nothing is
/// running (an empty summary).
///
/// Every unmount is forced: the sweep runs only when the daemon is not managing
/// its own teardown, where a non-force NFS unmount can block on a dead server.
#[cfg(not(target_os = "linux"))]
pub(crate) fn teardown_host_native_nfs(state_dir: &Path) -> anyhow::Result<TeardownSummary> {
    if !state_dir.exists() {
        return Ok(TeardownSummary::default());
    }

    let mut summary = TeardownSummary::default();
    let mut states = Vec::new();
    for entry in std::fs::read_dir(state_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
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

    let live_mount_points = states
        .iter()
        .filter(|(_, state)| pid_alive(state.pid))
        .map(|(_, state)| state.mount_point.clone())
        .collect::<HashSet<_>>();
    let mut seen_mount_points = HashSet::new();
    for (path, state) in states {
        if !pid_alive(state.pid) {
            tear_down_orphan(
                &path,
                &state.mount_point,
                live_mount_points.contains(&state.mount_point),
                &mut summary,
            );
        } else if seen_mount_points.insert(state.mount_point.clone()) {
            tear_down_one(&path, &state.mount_point, state.pid, &mut summary);
        }
    }

    Ok(summary)
}

/// Tear down one recorded mount and record the outcome in `summary`.
///
/// The unmount is always forced. The sweep is reached only when the daemon is
/// not managing its own teardown (it did not answer the control API), so a
/// non-force unmount can wait on an NFS server that has already vanished (a
/// `kill -9` leaves exactly such a stale mount). A forced unmount is safe for a
/// read-only projection.
#[cfg(not(target_os = "linux"))]
fn tear_down_one(state_file: &Path, mount_point: &Path, pid: u32, summary: &mut TeardownSummary) {
    unmount(mount_point, true);
    signal_term(pid);

    if !mount_settled(mount_point) {
        // Still mounted: keep the state file so a later `down` retries.
        summary.failed.push(mount_point.to_path_buf());
        return;
    }
    // Mount is gone. The daemon removes its own state file on clean exit;
    // remove it ourselves if it lingered or was orphaned.
    remove_state_file(state_file);
    summary.unmounted += 1;
}

#[cfg(not(target_os = "linux"))]
fn tear_down_orphan(
    state_file: &Path,
    mount_point: &Path,
    live_mount_exists: bool,
    summary: &mut TeardownSummary,
) {
    if live_mount_exists {
        remove_state_file(state_file);
        summary.swept_orphans += 1;
        return;
    }

    if omnifs_nfs::mount_is_active(mount_point) {
        unmount(mount_point, true);
        if !mount_settled(mount_point) {
            summary.failed.push(mount_point.to_path_buf());
            return;
        }
    }
    remove_state_file(state_file);
    summary.swept_orphans += 1;
}

/// Unmount the loopback view. `force` is used when the recording daemon is
/// already dead, so stale mounts use the platform's forced teardown path.
/// Output is swallowed: the authoritative signal is whether the mount survives
/// (see `mount_settled`), not the platform tool's exit text.
#[cfg(target_os = "macos")]
fn unmount(mount_point: &Path, force: bool) {
    let command = if force {
        UnmountCommand::forced(Platform::Macos, mount_point)
    } else {
        UnmountCommand::graceful(Platform::Macos, mount_point)
    };
    let _ = command.run_quiet();
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn force_unmount_host_native(mount_point: &Path) {
    unmount(mount_point, true);
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
        .is_ok_and(|status| status.success())
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

/// Poll until `mount_point` leaves the OS mount table, up to ~6s (a live daemon
/// needs a beat to unmount after SIGTERM).
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
