//! Live JSONL inspector TUI: trace reducer, sandbox map, and terminal
//! rendering for `omnifs inspect`.
//!
//! This crate owns everything about the inspector's own state and
//! rendering. It never depends on `omnifs-cli`: the CLI resolves daemon
//! connections and prints receipts, this crate only ever hands back plain
//! data ([`SessionReceipt`]) or a rendered frame.

mod app;
mod filter;
mod format;
mod keymap;
mod metrics;
mod run;
mod sandbox;
mod sandbox_ui;
mod source;
mod timeline;
mod trace_state;
mod tree;
mod ui;

pub use format::format_latency_us;
pub use run::{NonInteractiveFormat, SessionReceipt, run_plain, run_tui};
pub use source::SourceKind;
pub use trace_state::SlowOp;
