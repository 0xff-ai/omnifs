//! CLI grammar, JSON, and exit-code contract tests.

#![cfg(not(target_os = "wasi"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{install_test_provider, omnifs_bin, release_wasm_dir};
use omnifs_workspace::authn::CredentialId;
use omnifs_workspace::creds::{CredentialEntry, CredentialStore, FileStore};
use omnifs_workspace::mounts::Repository;
use secrecy::SecretString;
use time::OffsetDateTime;

struct Fixture {
    home: tempfile::TempDir,
    mount_point: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let mount_point = home.path().join("mnt");
        std::fs::create_dir_all(&mount_point).expect("mount point dir");
        std::fs::create_dir_all(home.path().join("mounts")).expect("mounts dir");
        std::fs::create_dir_all(home.path().join("providers")).expect("providers dir");
        Self { home, mount_point }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(omnifs_bin())
            .args(args)
            .env("OMNIFS_HOME", self.home_path())
            .env("OMNIFS_MOUNT_POINT", &self.mount_point)
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "warn")
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {}: {error}", args.join(" ")))
    }

    fn run_owned(&self, args: &[String]) -> Output {
        Command::new(omnifs_bin())
            .args(args)
            .env("OMNIFS_HOME", self.home_path())
            .env("OMNIFS_MOUNT_POINT", &self.mount_point)
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "warn")
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {}: {error}", args.join(" ")))
    }

    fn write_static_token_mount_without_credential(&self) {
        let provider_id = install_test_provider(&self.home_path().join("providers"));
        let spec = format!(
            r#"{{"provider":{{"id":"{provider_id}","meta":{{"name":"test-provider"}}}},"mount":"test","auth":{{"type":"static-token","scheme":"pat"}}}}"#
        );
        std::fs::write(self.home_path().join("mounts/test.json"), spec)
            .expect("write auth mount spec");
    }
}

fn install_web_provider(fixture: &Fixture) {
    let providers_dir = fixture.home_path().join("providers");
    let wasm = std::fs::read(release_wasm_dir().join("omnifs_provider_web.wasm"))
        .expect("read web provider wasm");
    let artifact =
        omnifs_workspace::provider::Artifact::from_bytes("omnifs_provider_web.wasm", wasm)
            .expect("validate web provider");
    let store = omnifs_workspace::provider::ProviderStore::new(&providers_dir);
    store.retain(&artifact).expect("retain web provider");
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
fn frontend_enable_help_requires_filesystem_and_lists_live_attachments_command() {
    let frontend = Command::new(omnifs_bin())
        .args(["frontend", "--help"])
        .output()
        .expect("spawn omnifs frontend --help");
    assert!(frontend.status.success());
    let frontend_help = String::from_utf8_lossy(&frontend.stdout);
    for command in ["enable", "disable", "restart", "ls"] {
        assert!(
            frontend_help.contains(command),
            "missing {command}: {frontend_help}"
        );
    }
    assert!(
        !frontend_help.contains(" up"),
        "retired frontend up in {frontend_help}"
    );
    assert!(
        !frontend_help.contains(" down"),
        "retired frontend down in {frontend_help}"
    );

    let enable = Command::new(omnifs_bin())
        .args(["frontend", "enable", "--help"])
        .output()
        .expect("spawn omnifs frontend enable --help");
    assert!(enable.status.success());
    let enable_help = String::from_utf8_lossy(&enable.stdout);
    assert!(enable_help.contains("<FILESYSTEM>"), "{enable_help}");
    assert!(enable_help.contains("--runtime <RUNTIME>"), "{enable_help}");
    for value in ["fuse", "nfs", "host", "docker", "libkrun"] {
        assert!(
            enable_help.contains(value),
            "missing {value} in {enable_help}"
        );
    }

    let shell = Command::new(omnifs_bin())
        .args(["frontend", "shell", "--help"])
        .output()
        .expect("spawn omnifs frontend shell --help");
    assert!(shell.status.success());
    let shell_help = String::from_utf8_lossy(&shell.stdout);
    assert!(shell_help.contains("--runtime <RUNTIME>"), "{shell_help}");
    assert!(shell_help.contains("<FILESYSTEM>"), "{shell_help}");
    assert!(
        !shell_help.contains("--mount"),
        "retired frontend shell --mount in {shell_help}"
    );

    let fixture = Fixture::new();
    let missing_args = fixture.run(&["frontend", "enable"]);
    assert_eq!(exit_code(&missing_args), 2, "{missing_args:?}");

    let removed_up = fixture.run(&["frontend", "up"]);
    assert_eq!(exit_code(&removed_up), 2, "{removed_up:?}");
    let stderr = String::from_utf8_lossy(&removed_up.stderr);
    assert!(stderr.contains("unrecognized subcommand 'up'"), "{stderr}");
}

#[test]
fn removed_top_level_commands_are_usage_errors() {
    let fixture = Fixture::new();
    // `init`, both snapshot command forms, and `frontend status` were deleted;
    // each must now be a clap usage error, not a silent no-op.
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
fn daemon_required_command_exits_3_when_control_socket_is_unreachable() {
    let fixture = Fixture::new();
    let output = fixture.run(&["inspect", "--plain"]);

    assert_eq!(exit_code(&output), 3);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("daemon not running"));
}

/// `omnifs up` validates every configured mount's host-managed credential
/// before spawning the daemon (see `Launcher::launch`'s `preflight_mounts`),
/// so a missing credential fails fast with exit code 4 and never reaches
/// `launch_native`, which is exactly what this test must not trigger.
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

    let status = fixture.run(&["status", "--output", "json"]);
    assert_eq!(exit_code(&status), 0);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["schema_version"], 1);
    assert_eq!(status_json["command"], "status");
    assert_eq!(status_json["verdict"], "ok");
    assert_eq!(status_json["result"]["daemon"]["probe"]["state"], "stopped");
    assert!(status_json["result"]["mounts"].as_array().is_some());
    assert!(status_json["result"].get("providers").is_none());

    let setup = fixture.run(&[
        "setup",
        "--no-up",
        "--providers",
        "dns",
        "--no-input",
        "--output",
        "jsonl",
    ]);
    assert_eq!(exit_code(&setup), 0);
    let setup_lines = String::from_utf8_lossy(&setup.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("setup JSONL line"))
        .collect::<Vec<_>>();
    assert_eq!(setup_lines.len(), 1);
    assert_eq!(setup_lines[0]["type"], "result");
    assert_eq!(setup_lines[0]["command"], "setup");
    assert_eq!(
        setup_lines[0]["result"]["mounts"].as_array().unwrap().len(),
        1
    );
    assert!(setup_lines[0]["result"].get("receipt").is_none());

    let mounts = fixture.run(&["mount", "ls", "--output", "json"]);
    assert_eq!(exit_code(&mounts), 0);
    let mounts_json = stdout_json(&mounts);
    assert!(mounts_json["result"]["mounts"].as_array().is_some());

    let version = fixture.run(&["version", "--output", "json"]);
    assert_eq!(exit_code(&version), 0);
    let version_json = stdout_json(&version);
    assert!(version_json["result"]["cli"].as_str().is_some());
    assert!(version_json["result"]["daemon"].is_null());
    assert!(version_json["result"]["channel"].as_str().is_some());
    assert!(version_json["result"].get("providers").is_none());
    assert!(version_json["paths"].is_null());

    let doctor = fixture.run(&["doctor", "--output", "json"]);
    assert_eq!(exit_code(&doctor), 5);
    let doctor_json = stdout_json(&doctor);
    assert_eq!(doctor_json["verdict"], "degraded");
    assert!(doctor_json["result"]["inventory"].is_object());
    assert!(doctor_json["result"]["findings"].as_array().is_some());
}

#[test]
fn lifecycle_json_receipts_emit_one_document_with_a_verdict() {
    let fixture = Fixture::new();

    // `down --output json` with no daemon settles a clean receipt on stdout and exits
    // 0; the prose rail stays on stderr.
    let down = fixture.run(&["down", "--output", "json"]);
    assert_eq!(
        exit_code(&down),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&down.stderr)
    );
    let down_json = stdout_json(&down);
    assert_eq!(down_json["command"], "down");
    assert!(matches!(
        down_json["verdict"].as_str(),
        Some("ok" | "degraded")
    ));
    assert!(down_json["result"]["rows"].as_array().is_some());
}

#[test]
fn destructive_mount_jsonl_commands_end_with_one_typed_result() {
    let fixture = Fixture::new();
    let terminal_result = |output: &Output| {
        assert_eq!(exit_code(output), 0, "{output:?}");
        let mut lines = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSONL line"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let terminal = lines.pop().unwrap();
        assert_eq!(terminal["type"], "result");
        terminal
    };
    fixture.write_static_token_mount_without_credential();
    let dry_run = fixture.run(&["mount", "rm", "test", "--dry-run", "--output", "jsonl"]);
    let terminal = terminal_result(&dry_run);
    assert_eq!(terminal["command"], "mount.rm");
    assert_eq!(terminal["result"]["mount"], "test");
    assert_eq!(terminal["result"]["dry_run"], true);
    assert!(
        terminal["result"]["rows"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    assert!(terminal["result"]["plan"]["rows"].as_array().is_some());

    let absent_dry_run = fixture.run(&["mount", "rm", "missing", "--dry-run", "--output", "jsonl"]);
    let terminal = terminal_result(&absent_dry_run);
    assert_eq!(terminal["command"], "mount.rm");
    assert_eq!(terminal["result"]["mount"], "missing");
    assert_eq!(terminal["result"]["dry_run"], true);
    assert!(
        terminal["result"]["rows"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );

    let test_spec = std::fs::read_to_string(fixture.home_path().join("mounts/test.json"))
        .expect("read test mount");
    std::fs::write(
        fixture.home_path().join("mounts/shared.json"),
        test_spec.replace("\"mount\":\"test\"", "\"mount\":\"shared\""),
    )
    .expect("write shared mount");
    let credential_id = CredentialId::new("test-provider", "pat", "default").unwrap();
    let store = FileStore::new(fixture.home_path().join("credentials.json"));
    store
        .put(
            &credential_id,
            &CredentialEntry::static_token(
                SecretString::from("secret".to_owned()),
                OffsetDateTime::UNIX_EPOCH,
            ),
        )
        .unwrap();

    let revoked = fixture.run(&["mount", "revoke", "test", "--yes", "--output", "jsonl"]);
    let terminal = terminal_result(&revoked);
    assert_eq!(terminal["command"], "mount.revoke");
    assert_eq!(terminal["result"]["rows"][0]["state"], "done");
    let result = terminal["result"]["rows"][0]["value"].as_str().unwrap();
    assert!(result.contains("test"));
    assert!(result.contains("shared"));
    assert!(store.get(&credential_id).unwrap().is_none());

    let already_absent = fixture.run(&["mount", "revoke", "test", "--output", "jsonl"]);
    let terminal = terminal_result(&already_absent);
    assert_eq!(terminal["result"]["rows"][0]["state"], "skip");
}

#[test]
fn mount_add_json_receipt_names_the_mount() {
    let fixture = Fixture::new();
    let mounts_dir = fixture.home_path().join("mounts");
    let sentinel = "CLI_SECRET_SENTINEL_MOUNT_ADD";
    let bootstrap = Repository::open(&mounts_dir)
        .expect("bootstrap mount repository")
        .head_revision()
        .expect("read bootstrap HEAD");

    let rejected = fixture.run(&[
        "mount",
        "add",
        "github",
        "--as",
        "dogfood-github",
        "--token",
        sentinel,
        "--no-validate",
        "--output",
        "json",
    ]);
    assert_ne!(exit_code(&rejected), 0);
    let rejected_json = stdout_json(&rejected);
    let message = rejected_json["error"]["message"]
        .as_str()
        .expect("structured rejection message");
    assert!(
        message.contains("--token VALUE is rejected")
            && message.contains("--token -")
            && message.contains("--token-env VAR"),
        "unexpected direct-token rejection: {message}"
    );
    let telemetry = std::fs::read_to_string(fixture.home_path().join("telemetry/cli.jsonl"))
        .expect("CLI telemetry record");
    for (label, bytes) in [
        ("stdout", rejected.stdout.as_slice()),
        ("stderr", rejected.stderr.as_slice()),
        ("workspace telemetry", telemetry.as_bytes()),
    ] {
        assert!(
            !String::from_utf8_lossy(bytes).contains(sentinel),
            "credential sentinel leaked into {label}"
        );
    }
    assert!(
        !mounts_dir.join("dogfood-github.json").exists(),
        "rejected mount add must not write a mount spec"
    );
    assert!(
        !fixture.home_path().join("credentials.json").exists(),
        "rejected mount add must not write credentials"
    );
    let after_rejection = Repository::open(&mounts_dir)
        .expect("reopen mount repository after rejection")
        .head_revision()
        .expect("read HEAD after rejection");
    assert_eq!(
        after_rejection, bootstrap,
        "rejection must not commit desired state"
    );

    let provider_id = install_test_provider(&fixture.home_path().join("providers"));
    let provider_id = provider_id.to_string();

    let output = fixture.run_owned(&[
        "mount".into(),
        "add".into(),
        provider_id,
        "--no-input".into(),
        "--yes".into(),
        "--output".into(),
        "json".into(),
    ]);
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
    assert_eq!(json["command"], "mount.add");
    assert_eq!(json["result"]["mount"], "test");
    assert!(
        matches!(
            json["result"]["status"].as_str(),
            Some("ready" | "sign_in_declined")
        ),
        "unexpected status: {json}"
    );
}

/// A structured command that fails before its final document emits exactly one JSON
/// error document on stdout carrying the stable `id`, not the human block.
#[test]
fn json_error_document_carries_a_stable_id() {
    let fixture = Fixture::new();
    fixture.write_static_token_mount_without_credential();

    let output = fixture.run(&["up", "--output", "json"]);
    assert_eq!(exit_code(&output), 4);
    let json = stdout_json(&output);
    assert_eq!(json["error"]["id"], "auth-required");
    assert!(json["error"]["message"].as_str().is_some());
}

#[test]
fn every_json_command_keeps_its_error_contract_before_workspace_resolution() {
    let commands: &[&[&str]] = &[
        &["status", "--output", "json"],
        &["mount", "ls", "--output", "json"],
        &["version", "--output", "json"],
        &["doctor", "--output", "json"],
        &["up", "--output", "json"],
        &["down", "--output", "json"],
        &[
            "mount",
            "add",
            "test",
            "--no-input",
            "--yes",
            "--output",
            "json",
        ],
    ];

    for args in commands {
        let output = Command::new(omnifs_bin())
            .args(*args)
            .env_remove("HOME")
            .env_remove("OMNIFS_HOME")
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "warn")
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {}: {error}", args.join(" ")));

        assert_eq!(exit_code(&output), 1, "{args:?}: {output:?}");
        let json = stdout_json(&output);
        assert_eq!(json["error"]["id"], "generic-failure", "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).lines().count(),
            1,
            "{args:?}"
        );
        assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
    }
}

#[test]
fn bare_invocation_without_mounts_points_to_mount_add() {
    let fixture = Fixture::new();
    let output = fixture.run(&[]);

    assert_eq!(exit_code(&output), 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Frontends  "));
    assert!(stdout.contains("Mounts  0"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("omnifs mount add <provider>"));
}

#[test]
fn mount_add_collision_renames_with_yes() {
    let fixture = Fixture::new();
    let provider_id = install_test_provider(&fixture.home_path().join("providers"));
    let provider_id = provider_id.to_string();

    let first = fixture.run_owned(&[
        "mount".into(),
        "add".into(),
        provider_id.clone(),
        "--no-input".into(),
        "--yes".into(),
    ]);
    assert_eq!(
        exit_code(&first),
        0,
        "first mount add must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(first.stdout.is_empty(), "session prose belongs on stderr");

    let second = fixture.run_owned(&[
        "mount".into(),
        "add".into(),
        provider_id,
        "--no-input".into(),
        "--yes".into(),
    ]);
    assert_eq!(
        exit_code(&second),
        0,
        "second mount add must rename the collision\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(second.stdout.is_empty(), "session prose belongs on stderr");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("┌ omnifs mount add"), "{stderr}");
    assert!(
        stderr.contains("mount name") && stderr.contains("test taken, using test-2"),
        "--yes collision rename must stay visible: {stderr}"
    );
    assert!(stderr.contains("└ Mounted `test-2`."), "{stderr}");
    assert!(
        fixture.home_path().join("mounts/test.json").is_file(),
        "first mount add must write the test mount spec"
    );
    assert!(
        fixture.home_path().join("mounts/test-2.json").is_file(),
        "collision rename must write the suggested mount spec"
    );
}

#[test]
fn mount_add_same_artifact_collision_preserves_existing_spec() {
    let fixture = Fixture::new();
    install_web_provider(&fixture);
    std::fs::remove_dir(fixture.home_path().join("mounts"))
        .expect("remove fixture mounts dir to exercise fresh-home bootstrap");
    assert!(!fixture.home_path().join("mounts").exists());

    let first = fixture.run(&[
        "mount",
        "add",
        "web",
        "--as",
        "web",
        "--no-input",
        "--no-auth",
        "--config-json",
        r#"{"domains":["example.com"]}"#,
    ]);
    assert_eq!(
        exit_code(&first),
        0,
        "first mount add on a fresh home failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let mount_path = fixture.home_path().join("mounts/web.json");
    let before = std::fs::read(&mount_path).expect("read first mount spec");

    let second = fixture.run(&[
        "mount",
        "add",
        "web",
        "--as",
        "web",
        "--no-input",
        "--no-auth",
        "--config-json",
        r#"{"domains":["example.org"]}"#,
    ]);
    assert_ne!(exit_code(&second), 0, "mount-name collision must fail");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("mount web already exists"), "{stderr}");
    assert!(
        stderr.contains("remove it first") && stderr.contains("different name"),
        "{stderr}"
    );
    assert_eq!(
        before,
        std::fs::read(&mount_path).expect("read unchanged mount spec")
    );
}

#[test]
fn mount_add_invalid_dynamic_domain_config_never_writes_spec() {
    let fixture = Fixture::new();
    install_web_provider(&fixture);
    let mount_path = fixture.home_path().join("mounts/web.json");
    let mounts_dir = fixture.home_path().join("mounts");
    let bootstrap = Repository::open(&mounts_dir)
        .expect("bootstrap mount repository")
        .head_revision()
        .expect("read bootstrap HEAD");

    for config in [
        r#"{"domains":[""]}"#,
        r#"{"domains":[" "]}"#,
        r#"{"domains":["."]}"#,
        r#"{"domains":["example.com/path"]}"#,
        r#"{"domains":["example.com:443"]}"#,
        r#"{"domains":["*"]}"#,
    ] {
        let output = fixture.run(&[
            "mount",
            "add",
            "web",
            "--as",
            "web",
            "--no-input",
            "--no-auth",
            "--config-json",
            config,
        ]);
        assert_ne!(exit_code(&output), 0, "invalid config must fail: {config}");
        assert!(
            !mount_path.exists(),
            "invalid config must not write {}",
            mount_path.display()
        );
    }

    let output = fixture.run(&[
        "mount",
        "add",
        "web",
        "--as",
        "web",
        "--no-input",
        "--no-auth",
        "--config-json",
        r#"{"domains":["example.com"]}"#,
        "--limits-json",
        r#"{"bogus":true}"#,
    ]);
    assert_ne!(exit_code(&output), 0, "unknown limits key must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("bogus"), "error must name bogus: {stderr}");
    assert!(
        !mount_path.exists(),
        "invalid limits must not write {}",
        mount_path.display()
    );
    assert!(
        !fixture.home_path().join("credentials.json").exists(),
        "invalid limits must not write credentials"
    );
    let after = Repository::open(&mounts_dir)
        .expect("reopen mount repository after rejection")
        .head_revision()
        .expect("read HEAD after rejection");
    assert_eq!(after, bootstrap, "rejection must not commit desired state");
}
