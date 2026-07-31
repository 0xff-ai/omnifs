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
fn resource_help_exposes_only_the_final_public_grammar() {
    let top = Command::new(omnifs_bin())
        .arg("--help")
        .output()
        .expect("spawn omnifs --help");
    assert!(top.status.success());
    let top_help = String::from_utf8_lossy(&top.stdout);
    for command in ["provider", "mount", "credential", "fs"] {
        assert!(top_help.contains(command), "missing {command}: {top_help}");
    }
    assert!(
        !top_help.contains("\n  attachment "),
        "retired command remains public: {top_help}"
    );

    for (group, expected) in [
        ("provider", ["add", "ls", "show", "rm"].as_slice()),
        (
            "mount",
            ["add", "update", "reauth", "revoke", "rm", "ls", "show"].as_slice(),
        ),
        (
            "credential",
            ["login", "set", "ls", "show", "rm", "revoke"].as_slice(),
        ),
        (
            "fs",
            ["add", "ls", "show", "rm", "restart", "shell"].as_slice(),
        ),
    ] {
        let output = Command::new(omnifs_bin())
            .args([group, "--help"])
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {group} --help: {error}"));
        assert!(output.status.success(), "{group}: {output:?}");
        let help = String::from_utf8_lossy(&output.stdout);
        for command in expected {
            assert!(help.contains(command), "missing {group} {command}: {help}");
        }
        for retired in ["attach", "detach", "create", "enable", "disable"] {
            assert!(
                !help
                    .lines()
                    .any(|line| line.trim_start().starts_with(retired)),
                "retired {group} {retired}: {help}"
            );
        }
    }

    let mount_add = Command::new(omnifs_bin())
        .args(["mount", "add", "--help"])
        .output()
        .expect("spawn omnifs mount add --help");
    assert!(mount_add.status.success());
    let mount_help = String::from_utf8_lossy(&mount_add.stdout);
    for retired in [
        "--name",
        "--provider",
        "--config",
        "--token",
        "--account",
        "--memory",
    ] {
        assert!(
            !mount_help.contains(retired),
            "flag-heavy authoring option {retired} remains: {mount_help}"
        );
    }

    let credential_set = Command::new(omnifs_bin())
        .args(["credential", "set", "--help"])
        .output()
        .expect("spawn omnifs credential set --help");
    assert!(credential_set.status.success());
    let credential_help = String::from_utf8_lossy(&credential_set.stdout);
    assert!(credential_help.contains("<NAME>"), "{credential_help}");
    assert!(credential_help.contains("--from-env"), "{credential_help}");
    assert!(!credential_help.contains("--token"), "{credential_help}");

    let shell = Command::new(omnifs_bin())
        .args(["fs", "shell", "--help"])
        .output()
        .expect("spawn omnifs fs shell --help");
    assert!(shell.status.success());
    let shell_help = String::from_utf8_lossy(&shell.stdout);
    for argument in ["<NAME>", "[COMMAND]"] {
        assert!(
            shell_help.contains(argument),
            "missing {argument}: {shell_help}"
        );
    }
    for retired in [
        "--name",
        "--protocol",
        "--runtime",
        "--mount",
        "--command",
        "--shell",
    ] {
        assert!(
            !shell_help.contains(retired),
            "retired {retired} in {shell_help}"
        );
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
fn interactive_mutation_refusal_points_automation_to_kcl() {
    let fixture = Fixture::new();
    for args in [
        ["provider", "add"].as_slice(),
        ["mount", "add"].as_slice(),
        ["fs", "add"].as_slice(),
        ["credential", "login"].as_slice(),
    ] {
        let output = fixture.run(args);
        assert_eq!(exit_code(&output), 4, "{args:?}: {output:?}");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let combined = combined.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            combined
                .contains("Use omnifs plan <file> and omnifs apply <file> --yes for automation."),
            "{args:?}: {combined}"
        );
    }
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
            ["attachment", "ls"].as_slice(),
            "unrecognized subcommand 'attachment'",
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
            ["fs", "attach", "main"].as_slice(),
            "unrecognized subcommand 'attach'",
        ),
        (
            ["fs", "detach", "main"].as_slice(),
            "unrecognized subcommand 'detach'",
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
