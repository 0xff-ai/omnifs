//! Workspace-local dogfood metrics.
//!
//! Local-only usage counters used to compute product kill criteria denominators
//! (mount sessions that survive without manual recovery; weekly-active use). The
//! privacy contract is non-negotiable and enforced here:
//!
//! - Records are written only under `<config_dir>/metrics/`, inside the user's
//!   own `OMNIFS_HOME`. The directory is created `0700` and every file `0600`.
//! - Nothing here is ever transmitted anywhere. This module performs no network
//!   I/O and pulls in no networking dependency; the only side effects are
//!   appending local event records and updating the health-nudge marker.
//! - Writes are strictly best-effort. Any failure (directory, permissions,
//!   serialization, or the write itself) is logged at `debug` and swallowed;
//!   metric recording never propagates an error into a real code path.
//!
//! The daemon and the CLI are the two writers. They share this appender and the
//! record schemas so the on-disk format has a single owner.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::debug;

use crate::io::ensure_private_dir;

/// Subdirectory of `config_dir` that holds the metrics JSONL files. The Bun
/// reporter (`scripts/bench/dogfood-report.ts`) hardcodes the same relative
/// path; keep the two in sync.
pub const SUBDIR: &str = "metrics";

/// Environment variable that turns metrics off for a process. Any of `0`,
/// `false`, `no`, or `off` (case-insensitive) disables it; anything else, or
/// being unset, leaves it on. The daemon reads this because no strict-config
/// channel reaches it; the CLI honors it too, as a global kill switch on
/// top of its `[metrics] enabled` config field, and propagates it to the
/// daemon it launches.
pub const ENV_SWITCH: &str = "OMNIFS_METRICS";

const DAEMON_FILE: &str = "daemon.jsonl";
const CLI_FILE: &str = "cli.jsonl";
const HEALTH_NUDGE_FILE: &str = "last-nudge";
const HEALTH_NUDGE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Read the process-wide metrics kill switch from [`ENV_SWITCH`]. Enabled
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

#[derive(Serialize)]
struct DaemonRecord<'a> {
    ts: &'a str,
    event: DaemonEvent,
    mounts: usize,
}

#[derive(Serialize)]
struct CliRecord<'a> {
    ts: &'a str,
    cmd: &'a str,
    exit: i32,
}

/// Appends dogfood records under `<config_dir>/metrics/`. Construct once and
/// reuse; every method is best-effort and never fails.
#[derive(Debug, Clone)]
pub struct Sink {
    dir: PathBuf,
    enabled: bool,
}

/// Persistent metrics component for one workspace. It owns both the event
/// sink and the health-nudge marker used by the CLI.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    #[must_use]
    pub(crate) fn new(config_dir: &Path) -> Self {
        Self {
            dir: config_dir.join(SUBDIR),
        }
    }

    #[must_use]
    pub fn sink(&self, enabled: bool) -> Sink {
        Sink {
            dir: self.dir.clone(),
            enabled,
        }
    }

    /// Whether the workspace health nudge has not been shown recently.
    #[must_use]
    pub fn health_nudge_due(&self) -> bool {
        let Ok(metadata) = std::fs::metadata(self.dir.join(HEALTH_NUDGE_FILE)) else {
            return true;
        };
        let Ok(modified) = metadata.modified() else {
            return true;
        };
        modified
            .elapsed()
            .map_or(true, |elapsed| elapsed >= HEALTH_NUDGE_INTERVAL)
    }

    /// Record that the workspace health nudge was shown.
    pub fn record_health_nudge(&self) -> std::io::Result<()> {
        ensure_private_dir(&self.dir)?;
        let path = self.dir.join(HEALTH_NUDGE_FILE);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        crate::io::write_atomic(&path, format!("{now}\n").as_bytes(), 0o600)
    }
}

impl Sink {
    /// Record a daemon lifecycle event to `daemon.jsonl`.
    pub fn daemon_event(&self, event: DaemonEvent, mounts: usize) {
        if !self.enabled {
            return;
        }
        self.append(
            DAEMON_FILE,
            &DaemonRecord {
                ts: &now_rfc3339(),
                event,
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
            // Never propagate: local metrics are a side channel, not a code path.
            debug!(%error, file, "dogfood metrics write skipped");
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

    fn sink(config_dir: &Path, enabled: bool) -> Sink {
        Store::new(config_dir).sink(enabled)
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn disabled_sink_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = sink(tmp.path(), false);
        sink.daemon_event(DaemonEvent::DaemonStart, 0);
        sink.cli_event("status", 0);
        assert!(
            !tmp.path().join(SUBDIR).exists(),
            "a disabled sink must not create the metrics directory"
        );
    }

    #[test]
    fn daemon_event_appends_one_json_line_per_call() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = sink(tmp.path(), true);
        sink.daemon_event(DaemonEvent::DaemonStart, 0);
        sink.daemon_event(DaemonEvent::FrontendServing, 3);

        let path = tmp.path().join(SUBDIR).join(DAEMON_FILE);
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "each call appends exactly one line");

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "daemon_start");
        assert_eq!(first["mounts"], 0);
        assert!(first["ts"].as_str().unwrap().contains('T'), "ts is rfc3339");

        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["event"], "frontend_serving");
        assert_eq!(second["mounts"], 3);
    }

    #[test]
    fn cli_event_records_cmd_and_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = sink(tmp.path(), true);
        sink.cli_event("up", 0);
        sink.cli_event("doctor", 2);

        let path = tmp.path().join(SUBDIR).join(CLI_FILE);
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let last: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(last["cmd"], "doctor");
        assert_eq!(last["exit"], 2);
    }

    #[test]
    fn health_nudge_marker_is_owned_by_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        assert!(store.health_nudge_due());

        store.record_health_nudge().unwrap();

        assert!(!store.health_nudge_due());
    }

    #[cfg(unix)]
    #[test]
    fn directory_is_0700_and_files_are_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let sink = store.sink(true);
        sink.daemon_event(DaemonEvent::DaemonStart, 0);
        sink.cli_event("status", 0);
        store.record_health_nudge().unwrap();

        let dir = tmp.path().join(SUBDIR);
        assert_eq!(mode_of(&dir), 0o700, "metrics dir must be private");
        assert_eq!(mode_of(&dir.join(DAEMON_FILE)), 0o600);
        assert_eq!(mode_of(&dir.join(CLI_FILE)), 0o600);
        assert_eq!(mode_of(&dir.join(HEALTH_NUDGE_FILE)), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn append_tightens_permissions_on_a_preexisting_loose_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DAEMON_FILE);
        std::fs::write(&path, b"{\"pre\":true}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let sink = sink(tmp.path(), true);
        sink.daemon_event(DaemonEvent::DaemonStop, 0);

        assert_eq!(mode_of(&path), 0o600, "a loose file is tightened on append");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            2,
            "append preserves the existing content"
        );
    }
}
