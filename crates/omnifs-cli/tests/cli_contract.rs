//! CLI grammar, JSON, and exit-code contract tests.

#![cfg(not(target_os = "wasi"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{install_test_provider, install_test_provider_as, omnifs_bin};

struct Fixture {
    home: tempfile::TempDir,
    mount_point: PathBuf,
    daemon_addr: &'static str,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let mount_point = home.path().join("mnt");
        std::fs::create_dir_all(&mount_point).expect("mount point dir");
        std::fs::create_dir_all(home.path().join("mounts")).expect("mounts dir");
        std::fs::create_dir_all(home.path().join("providers")).expect("providers dir");
        Self {
            home,
            mount_point,
            daemon_addr: "127.0.0.1:9",
        }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(omnifs_bin())
            .args(args)
            .env("OMNIFS_HOME", self.home_path())
            .env("OMNIFS_MOUNT_POINT", &self.mount_point)
            .env("OMNIFS_DAEMON_ADDR", self.daemon_addr)
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "warn")
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {}: {error}", args.join(" ")))
    }

    fn write_static_token_mount_without_credential(&self) {
        let provider_id = install_test_provider(&self.home_path().join("providers"));
        let spec = format!(
            r#"{{"provider":{{"id":"{provider_id}","meta":{{"name":"test-provider"}}}},"mount":"test","auth":{{"type":"static-token","scheme":"pat"}},"capabilities":{{"domains":["httpbin.org"]}}}}"#
        );
        std::fs::write(self.home_path().join("mounts/test.json"), spec)
            .expect("write auth mount spec");
    }
}

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
}

#[test]
fn removed_mounts_add_is_usage_error() {
    let fixture = Fixture::new();
    let output = fixture.run(&["mounts", "add", "github"]);

    assert_eq!(exit_code(&output), 2);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unrecognized subcommand 'add'"));
}

#[test]
fn daemon_required_command_exits_3_when_control_port_is_unreachable() {
    let fixture = Fixture::new();
    let output = fixture.run(&["inspect", "--plain"]);

    assert_eq!(exit_code(&output), 3);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("daemon not running"));
}

#[test]
fn missing_mount_credential_exits_4() {
    let fixture = Fixture::new();
    fixture.write_static_token_mount_without_credential();

    let output = fixture.run(&["up", "--runtime", "docker"]);

    assert_eq!(exit_code(&output), 4);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no stored credential"));
    assert!(stderr.contains("omnifs mounts reauth test"));
}

#[test]
fn mounts_reauth_requires_existing_mount() {
    let fixture = Fixture::new();
    let output = fixture.run(&["mounts", "reauth", "github", "--no-input"]);

    assert_eq!(exit_code(&output), 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no mount named `github`"));
}

#[test]
fn json_commands_emit_expected_shapes() {
    let fixture = Fixture::new();

    let status = fixture.run(&["status", "--json"]);
    assert_eq!(exit_code(&status), 0);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["runtime"]["state"], "not_running");
    assert!(status_json["mounts"].as_array().is_some());
    assert!(status_json["providers"].as_array().is_some());

    let mounts = fixture.run(&["mounts", "ls", "--json"]);
    assert_eq!(exit_code(&mounts), 0);
    let mounts_json = stdout_json(&mounts);
    assert!(mounts_json["mounts"].as_array().is_some());

    let providers = fixture.run(&["providers", "ls", "--json"]);
    assert_eq!(exit_code(&providers), 0);
    let providers_json = stdout_json(&providers);
    assert!(providers_json["local"].as_array().is_some());
    assert!(providers_json["daemon"].is_null());

    let version = fixture.run(&["version", "--json"]);
    assert_eq!(exit_code(&version), 0);
    let version_json = stdout_json(&version);
    assert!(version_json["cli"].as_str().is_some());
    assert!(version_json["daemon"].is_null());
    assert!(version_json["paths"]["config"].as_str().is_some());

    let doctor = fixture.run(&["doctor", "--json"]);
    let doctor_json = stdout_json(&doctor);
    assert!(doctor_json["verdict"].as_str().is_some());
    assert!(doctor_json["probes"].as_array().is_some());
    assert!(doctor_json["live"]["skipped"].as_str().is_some());
}

#[test]
fn bare_invocation_without_setup_points_to_setup() {
    let fixture = Fixture::new();
    let output = fixture.run(&[]);

    assert_eq!(exit_code(&output), 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("omnifs is not set up"));
    assert!(stdout.contains("omnifs setup"));
}

#[test]
fn scripted_setup_and_init_do_not_prompt() {
    let fixture = Fixture::new();
    install_test_provider_as(&fixture.home_path().join("providers"), "test");

    let setup = fixture.run(&["setup", "-y", "--no-up"]);
    assert_eq!(
        exit_code(&setup),
        0,
        "setup -y --no-up must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&setup.stdout),
        String::from_utf8_lossy(&setup.stderr)
    );

    let init = fixture.run(&["init", "test", "--no-input", "--yes"]);
    assert_eq!(
        exit_code(&init),
        0,
        "init test --no-input --yes must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(
        fixture.home_path().join("mounts/test.json").is_file(),
        "init must write the test mount spec"
    );
}
