//! The processes of a host NFS filesystem across a new instance and a new
//! daemon, through the CLI.
//!
//! - `omnifs fs restart` of a filesystem whose host instance answers `Restart`
//!   ends that instance without an unmount and starts a new one, which serves
//!   the mount in place with its filehandle table: the mount stays, a file in
//!   use keeps reading, and a fresh open reads.
//! - A host instance outlives `omnifs down --keep-filesystems` and reconnects
//!   to the next daemon.
//! - A runner runs no `mdutil` at the start or at the restart of its mount.
//!
//! Gated on `OMNIFS_ACCEPTANCE_LIVE`. Holds the cross-process NFS
//! serialization lock for the whole run and force-unmounts on teardown, so a
//! failed run never wedges a later one with an orphaned NFS mount.

#![cfg(not(target_os = "wasi"))]

use std::io::{Read, Seek, SeekFrom};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use omnifs_itest::live;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const READ_TIMEOUT: Duration = Duration::from_secs(15);

fn acceptance_gated() -> bool {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run the host restart acceptance");
        return false;
    }
    if !live::platform_can_mount() {
        eprintln!("skip: platform cannot mount");
        return false;
    }
    true
}

/// A profile with a daemon and the filesystem `live`, a host NFS mount of the
/// test provider, attached through the CLI. Its commands have first in their
/// `PATH` an `mdutil` that records each of its runs.
struct Profile {
    home: PathBuf,
    mount_point: PathBuf,
    path: std::ffi::OsString,
    daemon: Option<Child>,
    _dir: tempfile::TempDir,
    _lock: TcpListener,
}

impl Profile {
    fn attached() -> Self {
        let lock = live::nfs_serial_lock();
        // Under `/tmp`, so that the control socket of the host instance fits
        // the socket path limit of macOS.
        let dir = tempfile::Builder::new()
            .prefix("omnifs-")
            .tempdir_in("/tmp")
            .expect("profile folder");
        let home = dir.path().to_path_buf();
        let mount_point = home.join("mnt");
        std::fs::create_dir_all(&mount_point).expect("mount point");
        let path = recording_mdutil(&home);
        let mut profile = Self {
            home,
            mount_point,
            path,
            daemon: None,
            _dir: dir,
            _lock: lock,
        };
        profile.start_daemon();
        live::seed_test_namespace(&profile.home.join("control.sock"));
        let location = profile.mount_point.to_string_lossy().into_owned();
        profile.succeed(&[
            "fs",
            "create",
            "--name",
            "live",
            "--protocol",
            "nfs",
            "--runtime",
            "host",
            "--location",
            &location,
        ]);
        profile.succeed(&["fs", "attach", "--name", "live"]);
        profile
    }

    fn start_daemon(&mut self) {
        let daemon = Command::new(live::omnifs_bin())
            .arg("daemon")
            .env("OMNIFS_HOME", &self.home)
            .env("PATH", &self.path)
            .env("RUST_LOG", "warn")
            .spawn()
            .expect("spawn omnifs daemon");
        self.daemon = Some(daemon);
        let control_socket = self.home.join("control.sock");
        let deadline = Instant::now() + READY_TIMEOUT;
        while !live::control_ready(&control_socket) {
            assert!(Instant::now() < deadline, "the daemon never became ready");
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn omnifs(&self, args: &[&str]) -> Output {
        Command::new(live::omnifs_bin())
            .args(args)
            .env("OMNIFS_HOME", &self.home)
            .env("PATH", &self.path)
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "warn")
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {}: {error}", args.join(" ")))
    }

    fn succeed(&self, args: &[&str]) {
        let output = self.omnifs(args);
        assert!(
            output.status.success(),
            "omnifs {} failed with {}\nstdout: {}\nstderr: {}\nfilesystem log: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            std::fs::read_to_string(self.home.join("client/cache/filesystem-live.log"))
                .unwrap_or_default()
        );
    }

    fn status(&self) -> serde_json::Value {
        let output = self.omnifs(&["--output", "json", "status"]);
        serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .expect("status --output json must produce JSON")["result"]
            .clone()
    }

    /// The runs of the `mdutil` of the profile, a line each.
    fn mdutil_runs(&self) -> String {
        match std::fs::read_to_string(self.home.join("mdutil-runs")) {
            Ok(runs) => runs,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => panic!("read the runs of mdutil: {error}"),
        }
    }

    /// The record of the host instance of `live`.
    fn runner(&self) -> serde_json::Value {
        let record = self.home.join("client/filesystems/state/live/runner.json");
        serde_json::from_slice(&std::fs::read(record).expect("read the runner record"))
            .expect("runner record JSON")
    }

    fn finish(mut self) {
        self.succeed(&["fs", "detach", "--name", "live"]);
        self.succeed(&["down"]);
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.wait();
        }
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        force_unmount(&self.mount_point);
        let record = self.home.join("client/filesystems/state/live/runner.json");
        if let Ok(bytes) = std::fs::read(record)
            && let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(pid) = record["pid"].as_u64()
        {
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
        }
        if let Some(daemon) = self.daemon.as_mut() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

/// An `mdutil` under `home` that records each of its runs in
/// `{home}/mdutil-runs`, and the `PATH` that has it first.
fn recording_mdutil(home: &Path) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).expect("bin folder");
    let mdutil = bin.join("mdutil");
    std::fs::write(
        &mdutil,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\n",
            home.join("mdutil-runs").display()
        ),
    )
    .expect("write mdutil");
    std::fs::set_permissions(&mdutil, std::fs::Permissions::from_mode(0o755))
        .expect("make mdutil executable");
    let mut path = bin.into_os_string();
    if let Some(inherited) = std::env::var_os("PATH") {
        path.push(":");
        path.push(inherited);
    }
    path
}

/// Force-unmount `mount_point`: a forced unmount breaks a read parked on a
/// hard NFS mount whose server is gone, which a graceful unmount cannot.
fn force_unmount(mount_point: &Path) {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::OsStr;
        let mount_point = mount_point.as_os_str();
        let _ = Command::new("umount")
            .args([OsStr::new("-f"), mount_point])
            .status();
        let _ = Command::new("umount")
            .args([OsStr::new("-l"), mount_point])
            .status();
    }
    #[cfg(not(target_os = "linux"))]
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        while omnifs_nfs::mount_is_active(mount_point) && Instant::now() < deadline {
            let _ = Command::new("diskutil")
                .args(["unmount", "force"])
                .arg(mount_point)
                .status();
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

/// Reads `path` to its end through the mount, never longer than
/// `READ_TIMEOUT`: a read of a hard NFS mount without its server blocks.
fn read_path(path: &Path) -> Vec<u8> {
    let path = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::read(&path));
    });
    rx.recv_timeout(READ_TIMEOUT)
        .expect("a read of the mount must not hang")
        .expect("read through the mount")
}

/// Reads the held `file` from its start, never longer than `READ_TIMEOUT`.
fn read_held(file: &std::fs::File) -> Vec<u8> {
    let mut file = file.try_clone().expect("clone the held file");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = file
            .seek(SeekFrom::Start(0))
            .and_then(|_| file.read_to_end(&mut bytes))
            .map(|_| bytes);
        let _ = tx.send(result);
    });
    rx.recv_timeout(READ_TIMEOUT)
        .expect("a read of the held file must not hang")
        .expect("read the held file")
}

/// `omnifs fs restart` ends the host instance of an active NFS mount without
/// an unmount and starts a new one, which serves the mount in place with its
/// filehandle table: a file in use keeps reading, a fresh open reads, and
/// `omnifs status` gives the daemon and the new instance as running the
/// program of the command.
#[test]
fn a_host_instance_restarts_in_place_under_a_file_in_use() {
    if !acceptance_gated() {
        return;
    }
    // Declared before the profile, so that a failed run drops the held file
    // after the forced unmount of the profile, which a close of a file of a
    // mount without its server would wait for.
    let held;
    let profile = Profile::attached();
    let message = profile.mount_point.join("test/hello/message");
    held = std::fs::File::open(&message).expect("open the held file");
    let baseline = read_held(&held);
    assert!(!baseline.is_empty(), "the held file has bytes");
    let before = profile.runner();

    profile.succeed(&["fs", "restart", "--name", "live"]);

    assert!(
        omnifs_nfs::mount_is_active(&profile.mount_point),
        "the mount stays across a restart in place"
    );
    let after = profile.runner();
    assert_ne!(
        after["instance_id"], before["instance_id"],
        "a new instance serves the mount"
    );
    assert_eq!(read_held(&held), baseline, "the held file keeps reading");
    assert_eq!(read_path(&message), baseline, "a fresh open reads");
    let status = profile.status();
    assert_eq!(status["daemon"]["current_program"], true, "{status}");
    assert_eq!(
        status["filesystems"][0]["current_program"], true,
        "{status}"
    );
    assert_eq!(status["filesystems"][0]["state"], "attached", "{status}");
    drop(held);
    profile.finish();
}

/// A runner runs no `mdutil` at the start or at the restart of its mount: the
/// `mdutil` of the profile records no run, and the mount serves after each.
#[test]
fn a_runner_runs_no_mdutil_at_the_start_or_at_the_restart_of_its_mount() {
    if !acceptance_gated() {
        return;
    }
    let profile = Profile::attached();
    let message = profile.mount_point.join("test/hello/message");
    let baseline = read_path(&message);
    assert!(!baseline.is_empty(), "the mount serves after its start");

    profile.succeed(&["fs", "restart", "--name", "live"]);
    assert_eq!(
        read_path(&message),
        baseline,
        "the mount serves after its restart"
    );
    assert_eq!(profile.mdutil_runs(), "", "the runner runs no mdutil");
    profile.finish();
}

/// A host instance outlives `omnifs down --keep-filesystems`, which leaves its
/// mount, and reconnects to the next daemon.
#[test]
fn a_host_instance_outlives_down_keep_filesystems_and_reconnects_to_the_next_daemon() {
    if !acceptance_gated() {
        return;
    }
    let mut profile = Profile::attached();
    let message = profile.mount_point.join("test/hello/message");
    let baseline = read_path(&message);
    let before = profile.runner();

    profile.succeed(&["down", "--keep-filesystems"]);
    if let Some(mut daemon) = profile.daemon.take() {
        let _ = daemon.wait();
    }
    assert!(
        omnifs_nfs::mount_is_active(&profile.mount_point),
        "the mount outlives the daemon"
    );
    assert_eq!(
        profile.runner()["instance_id"],
        before["instance_id"],
        "the host instance outlives the daemon"
    );

    profile.start_daemon();
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let status = profile.status();
        if status["filesystems"][0]["state"] == "attached" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the host instance never reconnected to the next daemon: {status}"
        );
        std::thread::sleep(POLL_INTERVAL);
    }
    assert_eq!(
        profile.runner()["instance_id"],
        before["instance_id"],
        "the same instance serves the mount"
    );
    assert_eq!(read_path(&message), baseline, "a read after the reconnect");
    profile.finish();
}
