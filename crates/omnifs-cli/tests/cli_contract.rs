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
fn removed_top_level_commands_are_usage_errors() {
    let fixture = Fixture::new();
    // `init` folded into `mount add`, top-level `snapshot` into `mount
    // snapshot`, and `frontend status` was deleted; each must now be a clap
    // usage error, not a silent no-op.
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
            ["frontend", "status"].as_slice(),
            "unrecognized subcommand 'status'",
        ),
        (
            ["mounts", "ls"].as_slice(),
            "unrecognized subcommand 'mounts'",
        ),
        (
            ["providers", "ls"].as_slice(),
            "unrecognized subcommand 'providers'",
        ),
    ] {
        let output = fixture.run(args);
        assert_eq!(exit_code(&output), 2, "{args:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(needle), "{args:?}: {stderr}");
    }
}

#[test]
fn daemon_required_command_exits_3_when_control_port_is_unreachable() {
    let fixture = Fixture::new();
    let output = fixture.run(&["inspect", "--plain"]);

    assert_eq!(exit_code(&output), 3);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("daemon not running"));
}

/// `omnifs up` validates every configured mount's host-managed credential
/// before spawning the daemon (see `Launcher::launch`'s `preflight_mounts`),
/// so a missing credential fails fast with exit code 4 and never reaches
/// `launch_native` — this fixture's `OMNIFS_DAEMON_ADDR=127.0.0.1:9` would
/// make a real spawn attempt hang/fail loudly, which is exactly what this
/// test must not trigger.
#[test]
fn missing_mount_credential_exits_4() {
    let fixture = Fixture::new();
    fixture.write_static_token_mount_without_credential();

    let output = fixture.run(&["up"]);

    assert_eq!(exit_code(&output), 4);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no stored credential"));
    assert!(stderr.contains("omnifs mount reauth test"));
}

#[test]
fn mount_reauth_requires_existing_mount() {
    let fixture = Fixture::new();
    let output = fixture.run(&["mount", "reauth", "github", "--no-input"]);

    assert_eq!(exit_code(&output), 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The command name renders in the accent color, not backticks, so assert on
    // the message and the mount name separately.
    assert!(stderr.contains("no mount named"));
    assert!(stderr.contains("github"));
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

    let mounts = fixture.run(&["mount", "ls", "--json"]);
    assert_eq!(exit_code(&mounts), 0);
    let mounts_json = stdout_json(&mounts);
    assert!(mounts_json["mounts"].as_array().is_some());

    let providers = fixture.run(&["provider", "ls", "--json"]);
    assert_eq!(exit_code(&providers), 0);
    let providers_json = stdout_json(&providers);
    assert!(providers_json["local"].as_array().is_some());
    assert!(providers_json["daemon"].is_null());

    let version = fixture.run(&["version", "--json"]);
    assert_eq!(exit_code(&version), 0);
    let version_json = stdout_json(&version);
    assert!(version_json["cli"].as_str().is_some());
    assert!(version_json["daemon"].is_null());
    assert!(version_json["channel"].as_str().is_some());
    // Providers is now a structured object, not a bare count, and the paths
    // block moved to `doctor`.
    assert!(version_json["providers"]["state"].as_str().is_some());
    assert!(version_json["providers"]["count"].as_u64().is_some());
    assert!(version_json["paths"].is_null());

    let doctor = fixture.run(&["doctor", "--json"]);
    let doctor_json = stdout_json(&doctor);
    assert!(doctor_json["verdict"].as_str().is_some());
    assert!(doctor_json["probes"].as_array().is_some());
    assert!(doctor_json["live"]["skipped"].as_str().is_some());
}

#[test]
fn lifecycle_json_receipts_emit_one_document_with_a_verdict() {
    let fixture = Fixture::new();

    // `down --json` with no daemon settles a clean receipt on stdout and exits
    // 0; the prose rail stays on stderr.
    let down = fixture.run(&["down", "--json"]);
    assert_eq!(
        exit_code(&down),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&down.stderr)
    );
    let down_json = stdout_json(&down);
    assert_eq!(down_json["verdict"], "ok");
    assert!(down_json["rows"].as_array().is_some());

    // `reset --json --yes` with nothing configured is a clean, empty reset.
    let reset = fixture.run(&["reset", "--yes", "--json"]);
    assert_eq!(
        exit_code(&reset),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&reset.stderr)
    );
    let reset_json = stdout_json(&reset);
    assert_eq!(reset_json["verdict"], "ok");
    assert!(reset_json["rows"].as_array().is_some());
}

#[test]
fn mount_add_json_receipt_names_the_mount() {
    let fixture = Fixture::new();
    install_test_provider_as(&fixture.home_path().join("providers"), "test");

    let output = fixture.run(&["mount", "add", "test", "--no-input", "--yes", "--json"]);
    assert_eq!(
        exit_code(&output),
        0,
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The single JSON document is on stdout; the session rail is on stderr.
    let json = stdout_json(&output);
    assert_eq!(json["verdict"], "ok");
    assert_eq!(json["mount"], "test");
    assert!(
        matches!(json["status"].as_str(), Some("ready" | "sign_in_declined")),
        "unexpected status: {json}"
    );
}

/// A `--json` command that fails before its receipt emits exactly one JSON
/// error document on stdout carrying the stable `id`, not the human block.
#[test]
fn json_error_document_carries_a_stable_id() {
    let fixture = Fixture::new();
    fixture.write_static_token_mount_without_credential();

    let output = fixture.run(&["up", "--json"]);
    assert_eq!(exit_code(&output), 4);
    let json = stdout_json(&output);
    assert_eq!(json["error"]["id"], "auth-required");
    assert!(json["error"]["message"].as_str().is_some());
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
fn scripted_setup_and_mount_add_do_not_prompt() {
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
    assert!(setup.stdout.is_empty(), "session prose belongs on stderr");
    let setup_stderr = String::from_utf8_lossy(&setup.stderr);
    assert!(setup_stderr.contains("┌ omnifs setup"), "{setup_stderr}");
    assert!(setup_stderr.contains("1/4 environment"), "{setup_stderr}");
    assert!(
        setup_stderr.contains("2/4 what should omnifs mount?"),
        "{setup_stderr}"
    );
    assert!(setup_stderr.contains("└ You're set."), "{setup_stderr}");

    let init = fixture.run(&["mount", "add", "test", "--no-input", "--yes"]);
    assert_eq!(
        exit_code(&init),
        0,
        "mount add test --no-input --yes must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(init.stdout.is_empty(), "session prose belongs on stderr");
    let init_stderr = String::from_utf8_lossy(&init.stderr);
    assert!(init_stderr.contains("┌ omnifs mount add"), "{init_stderr}");
    assert!(
        init_stderr.contains("mount name") && init_stderr.contains("test taken, using test-2"),
        "--yes collision rename must stay visible: {init_stderr}"
    );
    assert!(init_stderr.contains("└ Mounted `test-2`."), "{init_stderr}");
    assert!(
        fixture.home_path().join("mounts/test.json").is_file(),
        "mount add must write the test mount spec"
    );
    assert!(
        fixture.home_path().join("mounts/test-2.json").is_file(),
        "collision rename must write the suggested mount spec"
    );
}
