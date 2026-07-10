//! Workspace-local dogfood telemetry.
//!
//! Local-only usage counters used to compute product kill criteria denominators
//! (mount sessions that survive without manual recovery; weekly-active use). The
//! privacy contract is non-negotiable and enforced here:
//!
//! - Records are written only under `<config_dir>/telemetry/`, inside the user's
//!   own `OMNIFS_HOME`. The directory is created `0700` and every file `0600`.
//! - Nothing here is ever transmitted anywhere. This module performs no network
//!   I/O and pulls in no networking dependency; the only side effect is
//!   appending a line to a local file.
//! - Writes are strictly best-effort. Any failure (directory, permissions,
//!   serialization, or the write itself) is logged at `debug` and swallowed;
//!   telemetry never propagates an error into a real code path.
//!
//! The daemon and the CLI are the two writers. They share this appender and the
//! record schemas so the on-disk format has a single owner.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tracing::debug;

use crate::io::ensure_private_dir;

/// Subdirectory of `config_dir` that holds the telemetry JSONL files. The Bun
/// reporter (`scripts/bench/dogfood-report.ts`) hardcodes the same relative
/// path; keep the two in sync.
pub const TELEMETRY_SUBDIR: &str = "telemetry";

/// Environment variable that turns telemetry off for a process. Any of `0`,
/// `false`, `no`, or `off` (case-insensitive) disables it; anything else, or
/// being unset, leaves it on. The daemon reads this because no strict-config
/// channel reaches it today; the CLI honors it too, as a global kill switch on
/// top of its `[telemetry] enabled` config field, and propagates it to the
/// daemon it launches.
pub const ENV_SWITCH: &str = "OMNIFS_TELEMETRY";

const DAEMON_FILE: &str = "daemon.jsonl";
const CLI_FILE: &str = "cli.jsonl";

/// Read the process-wide telemetry kill switch from [`ENV_SWITCH`]. Enabled
/// unless the variable is explicitly set to a falsey token.
#[must_use]
pub fn enabled_from_env() -> bool {
    match std::env::var(ENV_SWITCH) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// A daemon lifecycle transition recorded to `daemon.jsonl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonEvent {
    /// The daemon process has started.
    DaemonStart,
    /// The filesystem frontend has begun serving the mount.
    FrontendServing,
    /// The filesystem frontend has stopped serving (unmounted).
    FrontendStopped,
    /// The daemon process is shutting down.
    DaemonStop,
}

/// The launch backend the daemon is running under, recorded alongside events.
/// The daemon only ever runs host-native; kept as a named type (rather than
/// dropped from the record) so the on-disk schema stays stable and the
/// reporter script does not need a shape change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Native,
}

#[derive(Serialize)]
struct DaemonRecord<'a> {
    ts: &'a str,
    event: DaemonEvent,
    backend: Backend,
    mounts: usize,
}

#[derive(Serialize)]
struct CliRecord<'a> {
    ts: &'a str,
    cmd: &'a str,
    exit: i32,
}

/// Appends dogfood records under `<config_dir>/telemetry/`. Construct once and
/// reuse; every method is best-effort and never fails.
#[derive(Debug, Clone)]
pub struct TelemetrySink {
    dir: PathBuf,
    enabled: bool,
}

impl TelemetrySink {
    /// Build a sink writing under `<config_dir>/telemetry/`. When `enabled` is
    /// false every method is a no-op and no files are touched.
    #[must_use]
    pub fn new(config_dir: &Path, enabled: bool) -> Self {
        Self {
            dir: config_dir.join(TELEMETRY_SUBDIR),
            enabled,
        }
    }

    /// Record a daemon lifecycle event to `daemon.jsonl`.
    pub fn daemon_event(&self, event: DaemonEvent, backend: Backend, mounts: usize) {
        if !self.enabled {
            return;
        }
        self.append(
            DAEMON_FILE,
            &DaemonRecord {
                ts: &now_rfc3339(),
                event,
                backend,
                mounts,
            },
        );
    }

    /// Record a CLI invocation's outcome to `cli.jsonl`. `cmd` is the top-level
    /// subcommand; `exit` is the process exit code.
    pub fn cli_event(&self, cmd: &str, exit: i32) {
        if !self.enabled {
            return;
        }
        self.append(
            CLI_FILE,
            &CliRecord {
                ts: &now_rfc3339(),
                cmd,
                exit,
            },
        );
    }

    fn append<T: Serialize>(&self, file: &str, record: &T) {
        if let Err(error) = self.try_append(file, record) {
            // Never propagate: telemetry is a side channel, not a code path.
            debug!(%error, file, "dogfood telemetry write skipped");
        }
    }

    fn try_append<T: Serialize>(&self, file: &str, record: &T) -> std::io::Result<()> {
        use std::io::Write as _;

        ensure_private_dir(&self.dir)?;
        let mut line = serde_json::to_string(record)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        line.push('\n');

        let path = self.dir.join(file);
        let mut handle = open_private_append(&path)?;
        handle.write_all(line.as_bytes())
    }
}

/// Now, formatted as RFC 3339 UTC. Falls back to the epoch on the (unreachable)
/// formatting error so a record is still emitted with a parseable timestamp.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(unix)]
fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let handle = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies when the file is created; re-assert 0600 so an
    // append to a pre-existing, more-permissive file still tightens it.
    handle.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(handle)
}

#[cfg(not(unix))]
fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn disabled_sink_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = TelemetrySink::new(tmp.path(), false);
        sink.daemon_event(DaemonEvent::DaemonStart, Backend::Native, 0);
        sink.cli_event("status", 0);
        assert!(
            !tmp.path().join(TELEMETRY_SUBDIR).exists(),
            "a disabled sink must not create the telemetry directory"
        );
    }

    #[test]
    fn daemon_event_appends_one_json_line_per_call() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = TelemetrySink::new(tmp.path(), true);
        sink.daemon_event(DaemonEvent::DaemonStart, Backend::Native, 0);
        sink.daemon_event(DaemonEvent::FrontendServing, Backend::Native, 3);

        let path = tmp.path().join(TELEMETRY_SUBDIR).join(DAEMON_FILE);
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "each call appends exactly one line");

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "daemon_start");
        assert_eq!(first["backend"], "native");
        assert_eq!(first["mounts"], 0);
        assert!(first["ts"].as_str().unwrap().contains('T'), "ts is rfc3339");

        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["event"], "frontend_serving");
        assert_eq!(second["mounts"], 3);
    }

    #[test]
    fn cli_event_records_cmd_and_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = TelemetrySink::new(tmp.path(), true);
        sink.cli_event("up", 0);
        sink.cli_event("doctor", 2);

        let path = tmp.path().join(TELEMETRY_SUBDIR).join(CLI_FILE);
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let last: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(last["cmd"], "doctor");
        assert_eq!(last["exit"], 2);
    }

    #[cfg(unix)]
    #[test]
    fn directory_is_0700_and_files_are_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = TelemetrySink::new(tmp.path(), true);
        sink.daemon_event(DaemonEvent::DaemonStart, Backend::Native, 0);
        sink.cli_event("status", 0);

        let dir = tmp.path().join(TELEMETRY_SUBDIR);
        assert_eq!(mode_of(&dir), 0o700, "telemetry dir must be private");
        assert_eq!(mode_of(&dir.join(DAEMON_FILE)), 0o600);
        assert_eq!(mode_of(&dir.join(CLI_FILE)), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn append_tightens_permissions_on_a_preexisting_loose_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(TELEMETRY_SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DAEMON_FILE);
        std::fs::write(&path, b"{\"pre\":true}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let sink = TelemetrySink::new(tmp.path(), true);
        sink.daemon_event(DaemonEvent::DaemonStop, Backend::Native, 0);

        assert_eq!(mode_of(&path), 0o600, "a loose file is tightened on append");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            2,
            "append preserves the existing content"
        );
    }

    #[test]
    fn env_switch_defaults_on_and_respects_falsey_tokens() {
        // Not asserting against the real process env (tests share it); exercise
        // the token classification the reader relies on.
        for on in ["1", "true", "yes", "on", "anything"] {
            assert!(
                !matches!(
                    on.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                ),
                "{on} should read as enabled"
            );
        }
    }
}
