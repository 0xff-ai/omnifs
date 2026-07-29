//! CLI grammar, output, and exit-code contract tests.

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
fn fs_help_uses_named_instance_lifecycle_commands() {
    let fs = Command::new(omnifs_bin())
        .args(["fs", "--help"])
        .output()
        .expect("spawn omnifs fs --help");
    assert!(fs.status.success());
    let fs_help = String::from_utf8_lossy(&fs.stdout);
    for command in ["create", "rm", "attach", "detach", "restart", "shell", "ls"] {
        assert!(fs_help.contains(command), "missing {command}: {fs_help}");
    }
    for retired in ["enable", "disable", "delete"] {
        assert!(!fs_help.contains(retired), "retired {retired} in {fs_help}");
    }

    let create = Command::new(omnifs_bin())
        .args(["fs", "create", "--help"])
        .output()
        .expect("spawn omnifs fs create --help");
    assert!(create.status.success());
    let create_help = String::from_utf8_lossy(&create.stdout);
    for flag in ["--name", "--protocol", "--runtime", "--location"] {
        assert!(create_help.contains(flag), "missing {flag}: {create_help}");
    }

    let shell = Command::new(omnifs_bin())
        .args(["fs", "shell", "--help"])
        .output()
        .expect("spawn omnifs fs shell --help");
    assert!(shell.status.success());
    let shell_help = String::from_utf8_lossy(&shell.stdout);
    for flag in ["--name", "--shell", "[COMMAND]"] {
        assert!(shell_help.contains(flag), "missing {flag}: {shell_help}");
    }
    for retired in ["--protocol", "--runtime", "--mount", "--command"] {
        assert!(
            !shell_help.contains(retired),
            "retired {retired} in {shell_help}"
        );
    }

    let fixture = Fixture::new();
    let missing_args = fixture.run(&["fs", "attach"]);
    assert_eq!(exit_code(&missing_args), 2, "{missing_args:?}");

    let positional = fixture.run(&["fs", "attach", "main"]);
    assert_eq!(exit_code(&positional), 2, "{positional:?}");
    let stderr = String::from_utf8_lossy(&positional.stderr);
    assert!(stderr.contains("unexpected argument 'main'"), "{stderr}");
}

#[test]
fn fs_create_persists_one_resolved_spec_without_starting_a_runtime() {
    let fixture = Fixture::new();
    let location = fixture.home_path().join("named-host-mount");
    let args = vec![
        "fs".to_owned(),
        "create".to_owned(),
        "--name".to_owned(),
        "main".to_owned(),
        "--protocol".to_owned(),
        "nfs".to_owned(),
        "--runtime".to_owned(),
        "host".to_owned(),
        "--location".to_owned(),
        location.display().to_string(),
        "--output".to_owned(),
        "json".to_owned(),
    ];
    let created = fixture.run_owned(&args);
    assert_eq!(exit_code(&created), 0, "{created:?}");

    let spec_path = fixture
        .home_path()
        .join("client/filesystems/specs/main.json");
    let bytes = std::fs::read(&spec_path).expect("persisted filesystem spec");
    let spec: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        spec,
        serde_json::json!({
            "id": "main",
            "protocol": "nfs",
            "runtime": "host",
            "location": location,
        })
    );
    assert!(
        !fixture
            .home_path()
            .join("client/filesystems/state/main/runner.json")
            .exists(),
        "create must not launch a host runner"
    );

    let listed = fixture.run(&["fs", "ls", "--output", "json"]);
    assert_eq!(exit_code(&listed), 0, "{listed:?}");
    let listed = stdout_json(&listed);
    assert_eq!(listed["result"]["filesystems"][0]["id"], "main");
    assert_eq!(listed["result"]["filesystems"][0]["state"], "detached");

    let duplicate = fixture.run_owned(&args);
    assert_ne!(exit_code(&duplicate), 0, "{duplicate:?}");
    assert!(
        String::from_utf8_lossy(&duplicate.stdout).contains("already exists"),
        "{duplicate:?}"
    );
    assert_eq!(std::fs::read(&spec_path).unwrap(), bytes);
}

#[test]
fn fs_create_freezes_host_defaults_and_rejects_guest_locations() {
    let fixture = Fixture::new();
    let host = fixture.run(&[
        "fs",
        "create",
        "--name",
        "host-default",
        "--protocol",
        "nfs",
        "--runtime",
        "host",
        "--output",
        "json",
    ]);
    assert_eq!(exit_code(&host), 0, "{host:?}");
    let host_spec: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .home_path()
                .join("client/filesystems/specs/host-default.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        host_spec["location"],
        fixture
            .home_path()
            .join("client/filesystems/mounts/host-default")
            .display()
            .to_string()
    );

    let guest = fixture.run(&[
        "fs",
        "create",
        "--name",
        "guest",
        "--protocol",
        "fuse",
        "--runtime",
        "docker",
        "--location",
        "/tmp/not-owned-by-docker",
    ]);
    assert_ne!(exit_code(&guest), 0, "{guest:?}");
    assert!(
        String::from_utf8_lossy(&guest.stderr).contains("--location is not allowed"),
        "{guest:?}"
    );
    assert!(
        !fixture
            .home_path()
            .join("client/filesystems/specs/guest.json")
            .exists()
    );
}

#[test]
fn removed_top_level_commands_are_usage_errors() {
    let fixture = Fixture::new();
    for (args, needle) in [
        (
            ["init", "github"].as_slice(),
            "unrecognized subcommand 'init'",
        ),
        (
            ["snapshot", "test"].as_slice(),
            "unrecognized subcommand 'snapshot'",
        ),
        (
            ["mount", "snapshot", "test"].as_slice(),
            "unrecognized subcommand 'snapshot'",
        ),
        (
            ["filesystem", "ls"].as_slice(),
            "unrecognized subcommand 'filesystem'",
        ),
        (
            ["mounts", "ls"].as_slice(),
            "unrecognized subcommand 'mounts'",
        ),
        (
            ["providers", "ls"].as_slice(),
            "unrecognized subcommand 'providers'",
        ),
        (
            ["up", "--no-filesystem"].as_slice(),
            "unrecognized subcommand 'up'",
        ),
        (["down", "--force"].as_slice(), "--force"),
        (
            ["fs", "enable"].as_slice(),
            "unrecognized subcommand 'enable'",
        ),
        (
            ["shell", "--mount", "/tmp/omnifs"].as_slice(),
            "unrecognized subcommand 'shell'",
        ),
        (["status", "--json"].as_slice(), "--json"),
        (["status", "--progress", "json"].as_slice(), "--progress"),
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
    assert!(status_json["result"]["mounts"].as_array().is_some());
    assert_eq!(status_json["result"]["filesystems"], serde_json::json!([]));
    assert!(status_json["result"]["home"].is_string());
    assert!(status_json["result"]["daemon"].is_object());

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
