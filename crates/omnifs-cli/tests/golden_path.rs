//! Golden path: setup defaults, mount add fixture provider, up --wait, read, down.

#![cfg(not(target_os = "wasi"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use common::{
    force_unmount, free_port, install_test_provider_as, live_acceptance_enabled, nfs_serial_lock,
    omnifs_bin, platform_can_mount, recorded_pid, release_wasm_dir,
};

struct Fixture {
    home: tempfile::TempDir,
    mount_point: PathBuf,
    daemon_addr: String,
    daemon_pid: Option<u32>,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let providers = home.path().join("providers");
        std::fs::create_dir_all(&providers).expect("providers dir");
        install_test_provider_as(&providers, "test");

        let mount_point = home.path().join("mnt");
        std::fs::create_dir_all(&mount_point).expect("mount point dir");

        Self {
            home,
            mount_point,
            daemon_addr: "127.0.0.1:9".to_string(),
            daemon_pid: None,
        }
    }

    fn enable_live_addr(&mut self) {
        self.daemon_addr = format!("127.0.0.1:{}", free_port());
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(omnifs_bin())
            .args(args)
            .env("OMNIFS_HOME", self.home_path())
            .env("OMNIFS_MOUNT_POINT", &self.mount_point)
            .env("OMNIFS_DAEMON_ADDR", &self.daemon_addr)
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "warn")
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {}: {error}", args.join(" ")))
    }

    fn update_pid_from_record(&mut self) {
        self.daemon_pid = recorded_pid(self.home_path());
    }

    fn read_fixture_file(&self) -> Vec<u8> {
        // The daemon always runs host-native, so the mount is always
        // host-visible; there is no container fallback to fall back to.
        let host_path = self.mount_point.join("test/hello/message");
        std::fs::read(&host_path).unwrap_or_else(|error| {
            panic!(
                "read fixture file through host mount {}: {error}",
                host_path.display()
            )
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
        force_unmount(&self.mount_point);
    }
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(128)
}

fn record_wall_clock(home: &Path, elapsed: Duration) {
    let dir = home.join("telemetry");
    std::fs::create_dir_all(&dir).expect("telemetry dir");
    let path = dir.join("golden-path.json");
    let body = serde_json::json!({ "wall_clock_ms": elapsed.as_millis() });
    std::fs::write(path, format!("{body}\n")).expect("write golden path telemetry");
}

#[test]
fn setup_init_up_wait_read_down_golden_path() {
    let started = Instant::now();
    let mut fixture = Fixture::new();

    let setup = fixture.run(&["setup", "-y", "--no-up"]);
    assert_eq!(
        exit_code(&setup),
        0,
        "setup -y --no-up must exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&setup.stdout),
        String::from_utf8_lossy(&setup.stderr)
    );

    let init = fixture.run(&["mount", "add", "test", "--no-input", "--yes"]);
    assert_eq!(
        exit_code(&init),
        0,
        "mount add test --no-input --yes must exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(
        fixture.home_path().join("mounts/test.json").is_file(),
        "mount add must write mounts/test.json"
    );

    if !live_acceptance_enabled() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live golden-path mount checks");
        let elapsed = started.elapsed();
        record_wall_clock(fixture.home_path(), elapsed);
        assert!(elapsed < Duration::from_mins(2));
        return;
    }
    if !release_wasm_dir().join("test_provider.wasm").exists() {
        eprintln!("skip: test_provider.wasm missing (run `just build providers`)");
        let elapsed = started.elapsed();
        record_wall_clock(fixture.home_path(), elapsed);
        assert!(elapsed < Duration::from_mins(2));
        return;
    }
    if !platform_can_mount() {
        eprintln!("skip: platform cannot mount");
        let elapsed = started.elapsed();
        record_wall_clock(fixture.home_path(), elapsed);
        assert!(elapsed < Duration::from_mins(2));
        return;
    }

    let _guard = nfs_serial_lock();
    fixture.enable_live_addr();

    let up = fixture.run(&["up", "--wait", "30s"]);
    assert_eq!(
        exit_code(&up),
        0,
        "up --wait must exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&up.stdout),
        String::from_utf8_lossy(&up.stderr)
    );
    fixture.update_pid_from_record();

    assert_eq!(fixture.read_fixture_file(), b"Hello, world!");

    let down = fixture.run(&["down", "--force"]);
    assert_eq!(
        exit_code(&down),
        0,
        "down --force must exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&down.stdout),
        String::from_utf8_lossy(&down.stderr)
    );

    let elapsed = started.elapsed();
    record_wall_clock(fixture.home_path(), elapsed);
    assert!(
        elapsed < Duration::from_mins(2),
        "golden path took {elapsed:?}, expected under 120s"
    );
}
