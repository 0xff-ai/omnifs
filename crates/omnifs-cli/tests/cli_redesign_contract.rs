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

fn write_runner_observation(fixture: &Fixture, location: &Path) {
    let state_dir = fixture.home().join("cache/frontends/fuse/observed");
    std::fs::create_dir_all(&state_dir).expect("frontend state directory");
    std::fs::write(
        state_dir.join("mount-observed.json"),
        serde_json::json!({
            "version": 2,
            "mount_point": location.to_string_lossy().into_owned(),
            "pid": std::process::id(),
            "kind": "fuse"
        })
        .to_string(),
    )
    .expect("write frontend state fixture");
}

#[cfg(target_os = "macos")]
fn write_libkrun_observation(fixture: &Fixture) {
    let state_dir = fixture.home().join("libkrun");
    std::fs::create_dir_all(&state_dir).expect("libkrun state directory");
    std::fs::write(
        state_dir.join("libkrun.pid"),
        std::process::id().to_string(),
    )
    .expect("write libkrun pidfile");
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

fn write_warmup_observation(fixture: &Fixture, complete: bool) {
    let cache = fixture.home().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join("provider-warmup.json"),
        format!(
            "{{\"pid\":{},\"completed\":{},\"total\":1}}",
            std::process::id(),
            usize::from(complete)
        ),
    )
    .unwrap();
}

#[test]
fn cli_redesign_contract_human_status_has_context_and_resource_sections() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "human"]);
    let text = stdout_text(&output);

    assert!(text.contains("omnifs  "), "{text}");
    // The daemon is stopped in this fixture: the context metadata names
    // configured mounts rather than a stale pid/namespace (spec 3.10's
    // running-daemon shape, `daemon pid <pid>, serving <n> mounts, <m>
    // frontends`, is covered by the up/down lifecycle tests instead).
    assert!(text.contains("mounts configured"), "{text}");
    assert!(!text.contains("API -"), "{text}");
    for heading in ["Frontends  ", "Mounts  "] {
        assert!(text.contains(heading), "missing {heading:?} in {text}");
    }
}

#[test]
fn cli_redesign_contract_wide_headers_are_sentence_case_and_ordered() {
    let fixture = Fixture::new();
    write_runner_observation(&fixture, &fixture.mount_point);
    let output = fixture.run_with_env(&["status", "--output", "human"], &[("COLUMNS", "120")]);
    let text = stdout_text(&output);

    let filesystem = text.find("Filesystem").expect("filesystem header");
    let runtime = text.find("Runtime").expect("runtime header");
    let location = text.find("Location").expect("location header");
    let coverage = text.find("Coverage").expect("coverage header");
    let state = text.find("State").expect("state header");
    assert!(filesystem < runtime && runtime < location);
    assert!(location < coverage && coverage < state);
    assert!(!text.contains("FILESYSTEM"));
    assert!(!text.contains('|'));
    assert!(!text.contains("---"));
}

#[test]
fn cli_redesign_contract_frontends_report_whole_namespace_coverage() {
    let fixture = Fixture::new();
    write_runner_observation(&fixture, &fixture.mount_point);
    let output = fixture.run(&["status", "--output", "json"]);
    let json = stdout_json(&output);
    let frontends = json["result"]["frontends"].as_array().expect("frontends");
    assert_eq!(
        frontends.len(),
        1,
        "expected one runner observation: {json}"
    );
    assert_eq!(frontends[0]["scope"], "all");
    assert_eq!(frontends[0]["mount_count"], 0);

    let human = fixture.run(&["status", "--output", "human"]);
    let text = stdout_text(&human);
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
fn cli_redesign_contract_frontend_list_separates_support_from_instances() {
    let fixture = Fixture::new();

    let human = fixture.run(&["frontend", "ls", "--output", "human"]);
    let text = stdout_text(&human);
    let os = if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        std::env::consts::OS
    };
    assert!(
        text.contains(&format!("Supported frontends on {os}")),
        "{text}"
    );
    assert!(text.contains("Instantiated frontends  0"), "{text}");
    if cfg!(any(target_os = "macos", target_os = "linux")) {
        assert!(text.contains("multiple locations"), "{text}");
        assert!(text.contains("one per workspace"), "{text}");
    }

    let json_output = fixture.run(&["frontend", "ls", "--output", "json"]);
    let json = stdout_json(&json_output);
    let result = &json["result"];
    assert_eq!(result["platform"]["os"], std::env::consts::OS);
    assert_eq!(result["platform"]["arch"], std::env::consts::ARCH);
    assert!(result["supported_frontends"].is_array(), "{json}");
    assert_eq!(result["frontends"], serde_json::json!([]));
}

#[cfg(target_os = "macos")]
#[test]
fn cli_redesign_contract_reports_detached_libkrun_runner() {
    let fixture = Fixture::new();
    write_libkrun_observation(&fixture);

    let output = fixture.run(&["status", "--output", "json"]);
    let json = stdout_json(&output);
    let frontends = json["result"]["frontends"].as_array().expect("frontends");
    let libkrun = frontends
        .iter()
        .find(|frontend| frontend["runtime"] == "libkrun")
        .unwrap_or_else(|| panic!("missing libkrun runner: {json}"));
    assert_eq!(libkrun["filesystem"], "fuse");
    assert_eq!(libkrun["location"], "/omnifs");
    assert_eq!(libkrun["state"], "running");
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

    assert!(!text.lines().any(|line| line.trim() == "fix:"));
    assert!(text.contains("omnifs doctor"), "{text}");
    // Spec 2.10: a degraded row's recovery command reads `fix:  <command>`.
    assert!(
        text.lines()
            .any(|line| line.trim_start().starts_with("fix:  ")),
        "{text}"
    );
}

#[test]
fn cli_redesign_contract_narrow_status_uses_stacked_schema_fields() {
    let fixture = Fixture::new();
    write_runner_observation(&fixture, &fixture.mount_point);
    let output = fixture.run_with_env(&["status", "--output", "human"], &[("COLUMNS", "71")]);
    let text = stdout_text(&output);

    assert!(text.contains("Filesystem  Runtime  Location"));
    assert!(text.contains("Mounts  "), "{text}");
    assert!(text.contains("  /"), "identity field missing from {text}");
    assert!(
        text.lines()
            .any(|line| line.trim_start().starts_with("omnifs ")),
        "{text}"
    );
}

#[test]
fn cli_redesign_contract_status_json_exposes_observed_frontends_and_mounts() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "json"]);
    let json = stdout_json(&output);
    let result = &json["result"];

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "status");
    assert!(result["home"].is_string());
    assert!(result["daemon"].is_object());
    for key in ["frontends", "mounts"] {
        assert!(result[key].is_array(), "missing result.{key}: {json}");
    }
    assert!(
        result.get("runners").is_none(),
        "status must expose only canonical frontend rows"
    );
    assert!(result.get("providers").is_none());
    assert!(
        result.get("access").is_none(),
        "default status must keep access focused"
    );
}

#[test]
fn cli_redesign_contract_status_exposes_provider_warmup() {
    let fixture = Fixture::new();
    write_warmup_observation(&fixture, true);

    let output = fixture.run(&["status", "--output", "json"]);
    let json = stdout_json(&output);
    let warmup = &json["result"]["warmup"];
    assert_eq!(warmup["state"], "complete");
    assert_eq!(warmup["completed"], 1);
    assert_eq!(warmup["total"], 1);

    // A complete warmup is the steady state: JSON keeps the full status, the
    // human context strip omits it rather than narrating a non-event.
    let human = fixture.run(&["status", "--output", "human"]);
    assert!(!stdout_text(&human).contains("provider warmup"));
}

#[test]
fn cli_redesign_contract_marks_abandoned_warmup_interrupted() {
    let fixture = Fixture::new();
    write_warmup_observation(&fixture, false);

    let output = fixture.run(&["status", "--output", "json"]);
    let json = stdout_json(&output);
    assert_eq!(json["result"]["warmup"]["state"], "interrupted");
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
fn cli_redesign_contract_stopped_workspace_has_no_frontends() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "json"]);
    let json = stdout_json(&output);
    let frontends = json["result"]["frontends"].as_array().expect("frontends");
    assert!(
        frontends.is_empty(),
        "stopped workspace must have no frontends: {json}"
    );
}

#[test]
fn cli_redesign_contract_runner_observation_reports_exact_identity() {
    let fixture = Fixture::new();
    let runner_location = fixture.home().join("runner-mount");
    std::fs::create_dir_all(&runner_location).expect("runner mount directory");
    write_runner_observation(&fixture, &runner_location);
    let output = fixture.run(&["frontend", "ls", "--output", "json"]);
    let json = stdout_json(&output);
    let frontends = json["result"]["frontends"].as_array().expect("frontends");
    assert!(frontends.iter().any(|frontend| {
        frontend["filesystem"].as_str() == Some("fuse")
            && frontend["runtime"].as_str() == Some("host")
            && frontend["location"].as_str() == runner_location.to_str()
            && frontend["state"].as_str() == Some("running")
            && frontend["scope"] == "all"
            && frontend.get("source").is_none()
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

    assert_eq!(lines.len(), 1);
    assert!(lines.iter().all(|line| line["schema_version"] == 1));
    assert_eq!(lines.last().expect("terminal line")["type"], "result");
    assert_eq!(lines.last().expect("terminal line")["command"], "status");
    assert!(lines.iter().filter(|line| line["type"] == "result").count() == 1);
}

#[test]
fn cli_redesign_contract_state_rows_pair_symbols_with_lowercase_labels() {
    let fixture = Fixture::new();
    write_runner_observation(&fixture, &fixture.mount_point);
    let output = fixture.run(&["status", "--output", "human"]);
    let rendered = stdout_text(&output);
    let state_rows = rendered
        .lines()
        .filter(|line| line.contains("all 0 mounts"))
        .collect::<Vec<_>>();
    assert!(!state_rows.is_empty());
    assert!(
        state_rows.iter().all(|line| {
            line.contains("○ stopped") || line.contains("● attached") || line.contains("● running")
        }),
        "state rows must carry a symbol: {state_rows:?}"
    );
}

#[test]
fn cli_redesign_contract_frontend_config_is_rejected_as_removed_field() {
    let fixture = Fixture::new();
    write_frontend_config(&fixture, "[[frontends]]\nfilesystem = \"nfs\"\n");
    let output = fixture.run(&["up", "--output", "json"]);
    assert!(
        !output.status.success(),
        "removed frontend config must fail"
    );
    let json = stdout_json(&output);
    let message = json["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("frontends"), "{message}");
}

#[test]
fn cli_redesign_contract_old_commands_and_flags_are_usage_errors() {
    let fixture = Fixture::new();
    for args in [
        ["up", "--no-frontend"].as_slice(),
        ["down", "--force"].as_slice(),
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
fn cli_redesign_contract_json_removes_retired_control_fields() {
    let fixture = Fixture::new();
    let output = fixture.run(&["status", "--output", "json"]);
    let status = stdout_json(&output);
    assert!(status["result"]["workspace"]["api"].is_null());
    assert!(status["result"].get("mount").is_none());
    assert!(status["result"].get("mount_point").is_none());
}
