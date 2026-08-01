//! CLI grammar, output, and exit-code contract tests.
//!
//! Keep process coverage to representative executable checks. Exact help and
//! output registers live in `cli_transcripts`; parser details live in the CLI
//! unit tests.

#![cfg(not(target_os = "wasi"))]

mod common;

use std::process::{Command, Output};

use common::{CliFixture as Fixture, omnifs_bin};

fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(128)
}

fn stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout must be JSON: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn help_documents_exit_codes() {
    let output = Command::new(omnifs_bin())
        .arg("--help")
        .output()
        .expect("spawn omnifs --help");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Exit codes:"));
    assert!(stdout.contains("3  daemon unreachable"));
    assert!(stdout.contains("4  auth or consent required"));
    assert!(stdout.contains("5  degraded health"));
    assert!(stdout.contains("130  canceled"));
}

#[test]
fn public_resource_help_lists_the_final_command_groups() {
    let top = Command::new(omnifs_bin())
        .arg("--help")
        .output()
        .expect("spawn omnifs --help");
    assert!(top.status.success());
    let top_help = String::from_utf8_lossy(&top.stdout);
    for command in ["provider", "mount", "credential", "fs"] {
        assert!(top_help.contains(command), "missing {command}: {top_help}");
    }

    let output = Command::new(omnifs_bin())
        .args(["mount", "--help"])
        .output()
        .expect("spawn omnifs mount --help");
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8_lossy(&output.stdout);
    for command in ["add", "update", "reauth", "revoke", "rm", "ls", "show"] {
        assert!(help.contains(command), "missing mount {command}: {help}");
    }
}

#[test]
fn status_follow_requires_one_unambiguous_typed_target() {
    let fixture = Fixture::new();
    for args in [
        ["status", "--revision", "7"].as_slice(),
        ["status", "--action", "00000000000000000000000000000000"].as_slice(),
        [
            "status",
            "--follow",
            "--revision",
            "7",
            "--action",
            "00000000000000000000000000000000",
        ]
        .as_slice(),
    ] {
        let output = fixture.run(args);
        assert_eq!(exit_code(&output), 2, "{args:?}: {output:?}");
    }

    for args in [
        ["status", "--follow"].as_slice(),
        ["status", "--follow", "--revision", "7"].as_slice(),
        [
            "status",
            "--follow",
            "--action",
            "00000000000000000000000000000000",
        ]
        .as_slice(),
    ] {
        let output = fixture.run(args);
        assert_ne!(
            exit_code(&output),
            2,
            "valid follow grammar was rejected: {args:?}: {output:?}"
        );
    }
}

#[test]
fn representative_removed_commands_are_usage_errors() {
    let fixture = Fixture::new();
    for (args, needle) in [
        (
            ["init", "github"].as_slice(),
            "unrecognized subcommand 'init'",
        ),
        (
            ["fs", "attach", "main"].as_slice(),
            "unrecognized subcommand 'attach'",
        ),
        (["status", "--json"].as_slice(), "--json"),
    ] {
        let output = fixture.run(args);
        assert_eq!(exit_code(&output), 2, "{args:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(needle), "{args:?}: {stderr}");
    }
}

#[test]
fn daemon_required_command_exits_3_when_control_socket_is_unreachable() {
    let fixture = Fixture::new();
    let output = fixture.run(&["inspect", "--plain"]);

    assert_eq!(exit_code(&output), 3);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("daemon not running"));
}

#[test]
fn malformed_inspector_replay_is_a_line_numbered_failure() {
    let fixture = Fixture::new();
    let replay = fixture.home_path().join("replay.jsonl");
    std::fs::write(
        &replay,
        "{\"type\":\"dropped\",\"value\":{\"count\":1}}\nnot json\n",
    )
    .expect("write malformed replay");
    let output = fixture.run_owned(&[
        "inspect".into(),
        "--plain".into(),
        "--replay".into(),
        replay.to_string_lossy().into_owned(),
    ]);

    assert_eq!(exit_code(&output), 1, "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&replay.display().to_string()), "{stderr}");
    assert!(stderr.contains("line 2"), "{stderr}");
    assert!(stderr.contains("invalid json"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn inspector_replay_separates_human_plain_from_canonical_jsonl() {
    let fixture = Fixture::new();
    let replay = fixture.home_path().join("replay.jsonl");
    let contents = "{\"type\":\"dropped\",\"value\":{\"count\":1}}\n";
    std::fs::write(&replay, contents).expect("write replay");
    let output = fixture.run_owned(&[
        "inspect".into(),
        "--plain".into(),
        "--replay".into(),
        replay.to_string_lossy().into_owned(),
    ]);

    assert_eq!(exit_code(&output), 0, "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "dropped 1 events\n"
    );

    let output = fixture.run_owned(&[
        "--output".into(),
        "jsonl".into(),
        "inspect".into(),
        "--replay".into(),
        replay.to_string_lossy().into_owned(),
    ]);
    assert_eq!(exit_code(&output), 0, "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), contents);
}

#[test]
fn json_commands_emit_expected_shapes() {
    let fixture = Fixture::new();

    let status = fixture.run(&["status", "--output", "json"]);
    assert_eq!(exit_code(&status), 0);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["schema_version"], 1);
    assert_eq!(status_json["command"], "status");
    assert!(status_json["verdict"].is_string());
    for key in ["providers", "credentials", "mounts", "filesystems"] {
        assert!(
            status_json["result"][key].as_array().is_some(),
            "missing plural resource array {key}: {status_json}"
        );
    }
    assert!(status_json["result"]["inventory"]["home"].is_string());
    assert!(status_json["result"]["inventory"]["daemon"].is_object());

    let version = fixture.run(&["version", "--output", "json"]);
    assert_eq!(exit_code(&version), 0);
    let version_json = stdout_json(&version);
    assert!(version_json["result"]["cli"].as_str().is_some());
    assert!(version_json["result"]["channel"].as_str().is_some());
}

#[test]
fn lifecycle_json_receipts_emit_one_document_with_a_verdict() {
    let fixture = Fixture::new();
    let down = fixture.run(&["down", "--output", "json"]);
    assert_eq!(exit_code(&down), 0, "{down:?}");
    let down_json = stdout_json(&down);
    assert_eq!(down_json["command"], "down");
    assert!(down_json["verdict"].is_string());
    assert!(down_json["result"]["rows"].as_array().is_some());
}
