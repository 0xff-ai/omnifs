//! Stable end-to-end transcripts for the finite CLI presentation contract.
//!
//! Semantic assertions remain in `cli_contract.rs` and the command unit tests.
//! These snapshots catch changes to the complete stdout/stderr register.

#![cfg(not(target_os = "wasi"))]

mod common;

use std::process::Output;

use common::CliFixture as Fixture;

impl Fixture {
    fn transcript(&self, output: &Output) -> String {
        let home = self.home_path().to_string_lossy();
        let stdout = String::from_utf8_lossy(&output.stdout).replace(home.as_ref(), "$OMNIFS_HOME");
        let stderr = String::from_utf8_lossy(&output.stderr).replace(home.as_ref(), "$OMNIFS_HOME");
        format!(
            "exit: {}\nstdout:\n{stdout}stderr:\n{stderr}",
            output.status.code().unwrap_or(128)
        )
    }
}

#[test]
fn fresh_and_stopped_workspace_transcripts() {
    let fixture = Fixture::new();

    insta::assert_snapshot!("bare_fresh", fixture.transcript(&fixture.run(&[])));
    insta::assert_snapshot!(
        "status_stopped",
        fixture.transcript(&fixture.run(&["status"]))
    );
    insta::assert_snapshot!("down_stopped", fixture.transcript(&fixture.run(&["down"])));
}

#[test]
fn final_resource_grammar_transcripts() {
    let fixture = Fixture::new();

    insta::assert_snapshot!(
        "filesystem_help",
        fixture.transcript(&fixture.run(&["fs", "--help"]))
    );
    insta::assert_snapshot!(
        "provider_help",
        fixture.transcript(&fixture.run(&["provider", "--help"]))
    );
    insta::assert_snapshot!(
        "mount_add_help",
        fixture.transcript(&fixture.run(&["mount", "add", "--help"]))
    );
    insta::assert_snapshot!(
        "credential_set_help",
        fixture.transcript(&fixture.run(&["credential", "set", "--help"]))
    );
    insta::assert_snapshot!(
        "status_follow_conflict",
        fixture.transcript(&fixture.run(&[
            "status",
            "--follow",
            "--revision",
            "7",
            "--action",
            "00000000000000000000000000000000",
        ]))
    );
}

#[test]
fn logs_and_usage_error_transcripts() {
    let fixture = Fixture::new();

    insta::assert_snapshot!("logs_missing", fixture.transcript(&fixture.run(&["logs"])));
    insta::assert_snapshot!(
        "non_interactive_mutation_refusal",
        fixture.transcript(&fixture.run(&["fs", "add"]))
    );
}
