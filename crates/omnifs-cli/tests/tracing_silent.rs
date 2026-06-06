//! Verify that a no-args, default-verbosity invocation does not spam
//! INFO lines or span events to stderr when `RUST_LOG` is unset.

use std::process::Command;

#[test]
fn no_info_or_span_events_in_stderr_when_quiet() {
    let bin = std::env::var_os("NEXTEST_BIN_EXE_omnifs")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_omnifs"))
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_omnifs").into());
    let output = Command::new(bin)
        .arg("--help")
        .env_remove("RUST_LOG")
        .output()
        .expect("spawn omnifs --help");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // No INFO / NEW span lines when verbosity is zero and RUST_LOG is unset.
    assert!(
        !stderr.contains("INFO"),
        "stderr should not contain INFO at default verbosity; got:\n{stderr}"
    );
    assert!(
        !stderr.contains("close{"),
        "stderr should not contain span CLOSE events at default verbosity; got:\n{stderr}"
    );
}
