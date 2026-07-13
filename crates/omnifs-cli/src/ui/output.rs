//! Process-global output configuration for the machine contract.
//!
//! Three orthogonal switches shape every byte the toolkit emits, and none of
//! them can be threaded cleanly through the call graph (progress is driven deep
//! inside launch/teardown workflows). They are set once from the parsed CLI and
//! read by the renderers:
//!
//! - `--progress json` turns the progress stream into NDJSON on stdout instead
//!   of an animated stderr row.
//! - `-q`/`--quiet` drops conversational narration while keeping the record
//!   (settle rows, receipts, errors).
//! - a command's own `--json` flag records that it promises a single JSON
//!   document, so the top-level error handler emits a JSON error document
//!   instead of the human block when collection fails before that document.

use std::sync::atomic::{AtomicBool, Ordering};

static PROGRESS_JSON: AtomicBool = AtomicBool::new(false);
static QUIET: AtomicBool = AtomicBool::new(false);
static JSON_EXPECTED: AtomicBool = AtomicBool::new(false);

/// Record the two global flags parsed from the top-level CLI. Called once from
/// `main` before any command runs.
pub(crate) fn configure(progress_json: bool, quiet: bool) {
    PROGRESS_JSON.store(progress_json, Ordering::Relaxed);
    QUIET.store(quiet, Ordering::Relaxed);
}

/// A `--json` command announces itself so the error path emits a JSON error
/// document (with a stable `id`) rather than the human `Error:` block.
pub(crate) fn expect_json() {
    JSON_EXPECTED.store(true, Ordering::Relaxed);
}

pub(crate) fn progress_is_json() -> bool {
    PROGRESS_JSON.load(Ordering::Relaxed)
}

pub(crate) fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

pub(crate) fn json_expected() -> bool {
    JSON_EXPECTED.load(Ordering::Relaxed)
}
