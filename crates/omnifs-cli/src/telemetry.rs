//! CLI-side dogfood telemetry.
//!
//! Records one line per CLI invocation to the workspace-local `cli.jsonl`. The
//! writer, privacy contract, and record schema live in
//! [`omnifs_home::telemetry`]; this is the thin CLI adapter that resolves the
//! workspace and reads the `[telemetry] enabled` off-switch.
//!
//! It is called from every real exit point (main's return path, and the two
//! subcommands that `std::process::exit` themselves), so it must be
//! self-contained and never fail or block: a broken workspace or config simply
//! skips the record.

use crate::config::Config;

/// Record a completed CLI invocation. `cmd` is the top-level subcommand and
/// `exit` the process exit code. Best-effort; the internal `daemon` subcommand
/// is excluded by its callers (it records `daemon.jsonl` instead).
pub(crate) fn record_cli_exit(cmd: &str, exit: i32) {
    let Ok(layout) = omnifs_home::WorkspaceLayout::resolve() else {
        return;
    };
    // A malformed config disables telemetry rather than guessing: telemetry is
    // never allowed to surface an error, and off is the safe default.
    let enabled = Config::load(&layout.config_file).is_ok_and(|config| config.telemetry_enabled());
    omnifs_home::telemetry::TelemetrySink::new(&layout.config_dir, enabled).cli_event(cmd, exit);
}
