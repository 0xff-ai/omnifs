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
fn filesystem_legacy_list_transcript() {
    let fixture = Fixture::new();
    let specs = fixture.home_path().join("client/filesystems/specs");
    std::fs::create_dir_all(&specs).expect("legacy spec directory");
    let mount_point = fixture.home_path().join("mnt");
    std::fs::write(
        specs.join("dev.json"),
        serde_json::to_vec(&serde_json::json!({
            "id": "dev",
            "protocol": "nfs",
            "runtime": "host",
            "location": mount_point,
        }))
        .expect("legacy spec json"),
    )
    .expect("write legacy spec");

    let list = fixture.run(&["fs", "ls"]);
    insta::assert_snapshot!("fs_list_legacy", fixture.transcript(&list));
}

#[test]
fn logs_and_usage_error_transcripts() {
    let fixture = Fixture::new();

    insta::assert_snapshot!("logs_missing", fixture.transcript(&fixture.run(&["logs"])));
    insta::assert_snapshot!(
        "human_usage_error",
        fixture.transcript(&fixture.run(&["--output", "json", "fs", "attach"]))
    );
}
