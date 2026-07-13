//! Compile-safe black-box contracts for the shared-namespace CLI redesign.
//!
//! Black-box process/output contracts for the shared-namespace CLI surface.

#![cfg(not(target_os = "wasi"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::omnifs_bin;
use serde_json::Value;

struct Fixture {
    home: tempfile::TempDir,
    mount_point: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("workspace tempdir");
        let mount_point = home.path().join("host-mount");
        std::fs::create_dir_all(&mount_point).expect("host mount directory");
        std::fs::create_dir_all(home.path().join("mounts")).expect("mounts directory");
        std::fs::create_dir_all(home.path().join("providers")).expect("providers directory");
        Self { home, mount_point }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(omnifs_bin());
        command
            .args(args)
            .env("OMNIFS_HOME", self.home())
            .env("OMNIFS_MOUNT_POINT", &self.mount_point)
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .env("RUST_LOG", "warn");
        for (key, value) in env {
            command.env(key, value);
        }
        command
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {}: {error}", args.join(" ")))
    }
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout must be JSON: {error}\nstdout: {}\nstderr: {}",
            stdout_text(output),
            stderr_text(output)
        )
    })
}

fn json_lines(output: &Output) -> Vec<Value> {
    stdout_text(output)
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("invalid JSONL line {line:?}: {error}"))
        })
        .collect()
}

fn write_frontend_config(fixture: &Fixture, body: &str) {
    std::fs::write(fixture.home().join("config.toml"), body).expect("write config");
}

fn write_mount_fixture(fixture: &Fixture, name: &str) {
    std::fs::write(
        fixture.home().join(format!("mounts/{name}.json")),
        format!(
            r#"{{"provider":{{"id":"{}","meta":{{"name":"missing"}}}},"mount":"{}"}}"#,
            "0".repeat(64),
            name
        ),
    )
    .expect("write mount fixture");
}

#[test]
fn cli_redesign_contract_human_status_has_context_and_resource_sections() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "human"]);
    let text = stdout_text(&output);

    assert!(text.contains("omnifs  "), "{text}");
    assert!(text.contains("Daemon "), "{text}");
    assert!(text.contains("namespace /"), "{text}");
    assert!(!text.contains("API -"), "{text}");
    for heading in ["Frontends  ", "Mounts  "] {
        assert!(text.contains(heading), "missing {heading:?} in {text}");
    }
    let detail = fixture.run(&["status", "--detail", "--output", "human"]);
    assert!(stdout_text(&detail).contains("Providers  "));
}

#[test]
fn cli_redesign_contract_wide_headers_are_sentence_case_and_ordered() {
    let fixture = Fixture::new();
    let output = fixture.run_with_env(
        &["status", "--output", "human", "--detail"],
        &[("COLUMNS", "120")],
    );
    let text = stdout_text(&output);

    let filesystem = text.find("Filesystem").expect("filesystem header");
    let environment = text.find("Environment").expect("environment header");
    let location = text.find("Location").expect("location header");
    let coverage = text.find("Coverage").expect("coverage header");
    let state = text.find("State").expect("state header");
    assert!(filesystem < environment && environment < location);
    assert!(location < coverage && coverage < state);
    assert!(!text.contains("FILESYSTEM"));
    assert!(!text.contains('|'));
    assert!(!text.contains("---"));
}

#[test]
fn cli_redesign_contract_frontends_report_whole_namespace_coverage() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "human"]);
    let text = stdout_text(&output);
    let frontend_section = text.split("Mounts  ").next().unwrap_or(&text);
    let rows = frontend_section
        .lines()
        .filter(|line| line.contains("fuse") || line.contains("nfs"))
        .collect::<Vec<_>>();
    assert!(!rows.is_empty(), "no frontend rows in {text}");
    assert!(
        rows.iter().all(|row| row.contains("all 0 mounts")),
        "{rows:?}"
    );
    assert!(!frontend_section.contains("selected"));
}

#[test]
fn cli_redesign_contract_actions_are_contextual_rows_without_fix_column() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.home().join("mounts/broken.json"),
        r#"{"mount":"broken","provider":{"id":"00","meta":{"name":"missing"}}}"#,
    )
    .expect("write broken mount fixture");
    let output = fixture.run(&["status", "--output", "human"]);
    let text = stdout_text(&output);

    assert!(!text.lines().any(|line| line.trim() == "Fix"));
    assert!(text.contains("omnifs doctor"), "{text}");
    assert!(
        text.lines()
            .any(|line| line.trim_start().starts_with("Fix  ")),
        "{text}"
    );
}

#[test]
fn cli_redesign_contract_narrow_status_uses_stacked_schema_fields() {
    let fixture = Fixture::new();
    let output = fixture.run_with_env(&["status", "--output", "human"], &[("COLUMNS", "71")]);
    let text = stdout_text(&output);

    assert!(text.contains("Filesystem  Environment  Location"));
    assert!(text.contains("Mounts  "), "{text}");
    assert!(text.contains("  /"), "identity field missing from {text}");
    assert!(
        text.lines()
            .any(|line| line.trim_start().starts_with("omnifs ")),
        "{text}"
    );
}

#[test]
fn cli_redesign_contract_status_json_exposes_four_authoritative_resource_arrays() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "json"]);
    let json = stdout_json(&output);
    let result = &json["result"];

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "status");
    assert!(result["workspace"].is_object());
    for key in ["frontends", "mounts", "providers"] {
        assert!(result[key].is_array(), "missing result.{key}: {json}");
    }
    assert!(
        result.get("access").is_none(),
        "default status must keep access focused"
    );
}

#[test]
fn cli_redesign_contract_access_paths_form_mount_frontend_cross_product() {
    let fixture = Fixture::new();
    write_mount_fixture(&fixture, "example");
    let output = fixture.run(&["mount", "show", "example", "--output", "json"]);
    let json = stdout_json(&output);
    let access_paths = json["result"]["access_paths"]
        .as_array()
        .expect("mount show access_paths array");
    let frontend_count = json["result"]["frontends"].as_array().map_or(0, Vec::len);
    assert_eq!(access_paths.len(), frontend_count);
    assert!(access_paths.iter().all(|path| path["path"].is_string()));
}

#[test]
fn cli_redesign_contract_stopped_workspace_keeps_desired_frontends_visible() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "json"]);
    let json = stdout_json(&output);
    let frontends = json["result"]["frontends"].as_array().expect("frontends");
    assert!(!frontends.is_empty(), "{json}");
    assert!(frontends.iter().all(|frontend| {
        frontend["source"].is_string()
            && frontend["scope"] == "all"
            && frontend["state"].as_str() == Some("stopped")
    }));
}

#[test]
fn cli_redesign_contract_unmanaged_live_frontends_remain_observable() {
    let fixture = Fixture::new();
    let state_dir = fixture.home().join("cache/frontends/fuse/unmanaged");
    std::fs::create_dir_all(&state_dir).expect("frontend state directory");
    let unmanaged_location = fixture.home().join("unmanaged-mount");
    std::fs::create_dir_all(&unmanaged_location).expect("unmanaged mount directory");
    std::fs::write(
        state_dir.join("mount-unmanaged.json"),
        serde_json::json!({
            "version": 2,
            "mount_point": unmanaged_location,
            "pid": std::process::id(),
            "kind": "fuse"
        })
        .to_string(),
    )
    .expect("write unmanaged frontend state");
    let output = fixture.run(&["frontend", "ls", "--output", "json"]);
    let json = stdout_json(&output);
    let frontends = json["result"]["frontends"].as_array().expect("frontends");
    assert!(frontends.iter().any(|frontend| {
        frontend["source"].as_str() == Some("unmanaged")
            && frontend["state"].as_str() == Some("unmanaged")
            && frontend["scope"] == "all"
    }));
}

#[test]
fn cli_redesign_contract_status_has_no_singular_mount_projection() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "json"]);
    let json = stdout_json(&output);

    assert!(json["result"]["mounts"].is_array());
    assert!(json["result"]["frontends"].is_array());
    assert!(json["result"].get("mount").is_none());
    assert!(json.get("mount").is_none());
}

#[test]
fn cli_redesign_contract_jsonl_ends_with_one_terminal_result_envelope() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "jsonl"]);
    let lines = json_lines(&output);

    assert!(!lines.is_empty());
    assert!(lines.iter().all(|line| line["schema_version"] == 1));
    assert_eq!(lines.last().expect("terminal line")["type"], "result");
    assert_eq!(lines.last().expect("terminal line")["command"], "status");
    assert!(lines.iter().filter(|line| line["type"] == "result").count() == 1);
}

#[test]
fn cli_redesign_contract_structured_no_input_fails_before_prompt_bytes() {
    let fixture = Fixture::new();
    write_frontend_config(&fixture, "[[frontends]]\nkind = \"nfs\"\n");
    let output = fixture.run(&["status", "--output", "json", "--no-input"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stderr, b"", "structured errors must stay on stdout");
    let json = stdout_json(&output);
    assert_eq!(json["error"]["id"], "generic-failure");
    assert_eq!(stdout_text(&output).lines().count(), 1);
    assert!(!stdout_text(&output).contains("Continue?"));
}

#[test]
fn cli_redesign_contract_state_rows_pair_symbols_with_lowercase_labels() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "human"]);
    let rendered = stdout_text(&output);
    let state_rows = rendered
        .lines()
        .filter(|line| line.contains("all 0 mounts"))
        .collect::<Vec<_>>();
    assert!(!state_rows.is_empty());
    assert!(
        state_rows
            .iter()
            .all(|line| line.contains("○ stopped") || line.contains("● attached")),
        "state rows must carry a symbol: {state_rows:?}"
    );
}

#[test]
fn cli_redesign_contract_new_frontend_config_keys_parse_as_filesystem_environment_location() {
    let fixture = Fixture::new();
    write_frontend_config(
        &fixture,
        &format!(
            "[[frontends]]\nfilesystem = \"nfs\"\nenvironment = \"host\"\nlocation = \"{}\"\n",
            fixture.mount_point.display()
        ),
    );
    let output = fixture.run(&["status", "--output", "json"]);
    let json = stdout_json(&output);
    let frontend = &json["result"]["frontends"][0];
    assert_eq!(frontend["filesystem"], "nfs");
    assert_eq!(frontend["environment"], "host");
    assert_eq!(
        frontend["location"],
        fixture.mount_point.to_string_lossy().as_ref()
    );
}

#[test]
fn cli_redesign_contract_old_frontend_config_keys_are_rejected_with_replacements() {
    let fixture = Fixture::new();
    write_frontend_config(
        &fixture,
        &format!(
            "[[frontends]]\nkind = \"nfs\"\ndriver = \"local\"\nmount_point = \"{}\"\n",
            fixture.mount_point.display()
        ),
    );
    let output = fixture.run(&["status", "--output", "json"]);
    let json = stdout_json(&output);
    let message = json["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("filesystem"), "{message}");
    assert!(message.contains("environment"), "{message}");
    assert!(message.contains("location"), "{message}");
}

#[test]
fn cli_redesign_contract_old_commands_and_flags_are_usage_errors() {
    let fixture = Fixture::new();
    for args in [
        ["frontend", "up", "fuse", "--driver", "docker"].as_slice(),
        ["frontend", "down"].as_slice(),
        ["shell", "--mount", "/tmp/omnifs"].as_slice(),
        ["status", "--json"].as_slice(),
        ["status", "--progress", "json"].as_slice(),
    ] {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        let stderr = stderr_text(&output);
        assert!(
            stderr.contains("Usage") || stderr.contains("unrecognized"),
            "{stderr}"
        );
    }
}

#[test]
fn cli_redesign_contract_api_6_json_and_openapi_remove_singular_fields() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "json"]);
    let status = stdout_json(&output);
    assert!(
        status["result"]["workspace"]["api"].is_null()
            || status["result"]["workspace"]["api"] == "6.0",
        "offline status may omit API; a live status must report API 6.0: {status}"
    );
    assert!(status["result"].get("mount").is_none());
    assert!(status["result"].get("mount_point").is_none());

    let openapi_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../omnifs-api/openapi/daemon.json");
    let openapi: Value =
        serde_json::from_slice(&std::fs::read(&openapi_path).expect("checked-in OpenAPI document"))
            .expect("OpenAPI JSON");
    assert_eq!(openapi["info"]["version"], "6.0");
    let schemas = &openapi["components"]["schemas"];
    for schema in ["DaemonStatus", "StopReport"] {
        assert!(
            schemas[schema]["properties"].get("mount_point").is_none(),
            "{schema} still exposes mount_point"
        );
    }
    assert!(
        schemas["ProviderSummary"]["properties"]
            .get("latest")
            .is_none(),
        "ProviderSummary still exposes latest"
    );
    assert!(
        schemas["FrontendInfo"]["properties"]
            .get("mount_point")
            .is_some(),
        "FrontendInfo must retain its per-frontend mount_point"
    );
}
