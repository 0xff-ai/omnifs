//! Stable end-to-end transcripts for the finite CLI presentation contract.
//!
//! Semantic assertions remain in `cli_contract.rs` and the command unit tests.
//! These snapshots catch changes to the complete stdout/stderr register.

#![cfg(not(target_os = "wasi"))]

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::omnifs_bin;

struct Fixture {
    home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home tempdir"),
        }
    }

    fn path(&self) -> &Path {
        self.home.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(omnifs_bin())
            .args(args)
            .env("OMNIFS_HOME", self.path())
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "warn")
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {}: {error}", args.join(" ")))
    }

    fn transcript(&self, output: &Output) -> String {
        let home = self.path().to_string_lossy();
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
fn filesystem_create_and_list_transcripts() {
    let fixture = Fixture::new();
    let mount_point = fixture.path().join("mnt");
    std::fs::create_dir_all(&mount_point).expect("mount point");
    let mount_point = mount_point.to_string_lossy();

    let create = fixture.run(&[
        "fs",
        "create",
        "--name",
        "dev",
        "--protocol",
        "nfs",
        "--runtime",
        "host",
        "--location",
        &mount_point,
    ]);
    insta::assert_snapshot!("fs_create", fixture.transcript(&create));

    let list = fixture.run(&["fs", "ls"]);
    insta::assert_snapshot!("fs_list_detached", fixture.transcript(&list));
}

#[test]
fn logs_and_usage_error_transcripts() {
    let fixture = Fixture::new();

    insta::assert_snapshot!("logs_missing", fixture.transcript(&fixture.run(&["logs"])));
    insta::assert_snapshot!(
        "human_usage_error",
        fixture.transcript(&fixture.run(&["fs", "attach"]))
    );
    insta::assert_snapshot!(
        "json_usage_error",
        fixture.transcript(&fixture.run(&["--output", "json", "fs", "attach"]))
    );
}
