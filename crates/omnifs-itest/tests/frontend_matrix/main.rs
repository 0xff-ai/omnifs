//! Frontend conformance matrix: the product-contract toolbox run against a live
//! omnifs mount, once per frontend column, emitting a JSON scorecard and a
//! rendered markdown table per lane.
//!
//! Supersedes the single-purpose `omnifs-cli` `frontend_conformance` test. The
//! scorecards are the evidence base for the default-runtime and write-path
//! decisions, so every lane always writes its scorecard and prints its table
//! before asserting — a red run still leaves evidence.
//!
//! Lanes:
//! - `native_frontend_matrix` (env `OMNIFS_ACCEPTANCE_LIVE`): the daemon with
//!   the platform-default host-native frontend (kernel FUSE on Linux, `NFSv4`
//!   loopback on macOS). Column is cfg-selected.

#![cfg(not(target_os = "wasi"))]

use omnifs_itest::live;
use omnifs_itest::matrix::{self, Column, Exec, ROWS};

/// The platform-default native column for this OS.
#[cfg(target_os = "linux")]
const NATIVE_COLUMN: &Column = &matrix::LINUX_FUSE_NATIVE;
#[cfg(target_os = "macos")]
const NATIVE_COLUMN: &Column = &matrix::MACOS_NFS_LOOPBACK;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const NATIVE_COLUMN: &Column = &matrix::LINUX_FUSE_NATIVE;

/// Write the scorecard and print the table, then assert every row matched its
/// expectation. Evidence lands before the assertion so a red run is diagnosable.
fn finish(scorecard: &matrix::Scorecard) {
    let path = matrix::write_scorecard(scorecard);
    let table = matrix::render_table(std::slice::from_ref(scorecard));
    eprintln!("scorecard: {}", path.display());
    eprintln!("\n{table}");
    let mismatches = matrix::mismatches(scorecard);
    assert!(
        mismatches.is_empty(),
        "frontend matrix column `{}` has {} expectation mismatch(es):\n  {}",
        scorecard.column,
        mismatches.len(),
        mismatches.join("\n  ")
    );
}

#[test]
fn native_frontend_matrix() {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live-mount acceptance tests");
        return;
    }
    let Some(daemon) = live::start_native_daemon() else {
        return;
    };

    let scratch = tempfile::tempdir().expect("scratch dir");
    let exec = Exec::Local {
        root: daemon.tree_root(),
        scratch: scratch.path().to_path_buf(),
    };

    let scorecard = matrix::run_column(&exec, NATIVE_COLUMN, ROWS);
    finish(&scorecard);
}
