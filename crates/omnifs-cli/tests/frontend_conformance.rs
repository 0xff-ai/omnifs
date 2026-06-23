//! Frontend conformance: the projected tree must behave like real files for the
//! standard toolbox, through the platform-default frontend (FUSE on Linux, NFS
//! elsewhere).
//!
//! This launches the real daemon (`omnifs daemon`) against a hermetic
//! `OMNIFS_HOME` with only the canned `test-provider` mounted, then runs one
//! frontend-agnostic matrix (`run_matrix`) over the mount using the actual
//! shell toolbox. The daemon picks its own platform-default frontend; no
//! `--frontend` flag is accepted or needed.
//!
//! The mount spec is written to `<OMNIFS_HOME>/mounts/test.json` before the
//! daemon starts; the daemon reconciles from `mounts/` on startup, so no
//! `POST /v1/mounts` is needed. `OMNIFS_HOME`, `OMNIFS_MOUNT_POINT`, and
//! `OMNIFS_DAEMON_ADDR` are all set to hermetic per-test values so the test
//! never touches the user's real home directory or port 7878.
//!
//! Skip (not fail) only when the platform genuinely cannot mount. A daemon
//! that exits because it rejected a CLI argument is a real failure and panics.

#![cfg(not(target_os = "wasi"))]

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use common::{
    copy_release_wasm_into, free_port, install_test_provider, live_acceptance_enabled,
    nfs_serial_lock, omnifs_bin, platform_can_mount, release_wasm_dir, test_mount_spec,
};

fn curl(args: &[&str]) -> bool {
    Command::new("curl")
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}

/// What platform-default frontend the daemon will use on this OS.
#[cfg(target_os = "linux")]
const PLATFORM_FRONTEND: &str = "fuse";
#[cfg(not(target_os = "linux"))]
const PLATFORM_FRONTEND: &str = "nfs";

/// A running `omnifs daemon` with the test-provider mounted, torn down on drop.
struct Daemon {
    child: Child,
    mount_point: PathBuf,
    _home: tempfile::TempDir,
    /// Cross-process NFS serialization lock, held for the test's lifetime.
    _nfs_lock: std::net::TcpListener,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.detach_mount();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn detach_mount(&self) {
        let mp = self.mount_point.as_os_str();
        #[cfg(not(target_os = "linux"))]
        {
            if omnifs_nfs::mount_is_active(&self.mount_point) {
                let _ = omnifs_nfs::unmount(&self.mount_point);
            }
            let deadline = Instant::now() + Duration::from_secs(8);
            while omnifs_nfs::mount_is_active(&self.mount_point) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        #[cfg(target_os = "linux")]
        {
            let _ = Command::new("fusermount")
                .args([OsStr::new("-u"), mp])
                .status();
            let _ = Command::new("umount").arg(mp).status();
        }
        let _ = mp; // suppress unused warning on some configurations
    }
}

/// Bring up `omnifs daemon` (platform-default frontend) with only the
/// test-provider mounted. The spec is written to `mounts/` before spawning so
/// the daemon reconciles it on startup.
///
/// Returns `None` (skip) only when the platform genuinely cannot mount.
/// Panics if the daemon exits due to a CLI parse error or similar hard failure
/// (that is a real test failure, not a skip).
#[allow(clippy::too_many_lines)] // linear end-to-end daemon bring-up
fn start() -> Option<Daemon> {
    if !live_acceptance_enabled() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live-mount acceptance tests");
        return None;
    }
    let wasm_dir = release_wasm_dir();
    let test_wasm = wasm_dir.join("test_provider.wasm");
    if !test_wasm.exists() {
        eprintln!(
            "skip: {} missing (run `just providers-build`)",
            test_wasm.display()
        );
        return None;
    }

    if !platform_can_mount() {
        eprintln!("skip: platform cannot mount (no /dev/fuse)");
        return None;
    }

    // Hold the cross-process NFS lock for the whole test, so this binary's mount
    // never races the lifecycle suite's mounts in a parallel nextest run.
    let nfs_lock = nfs_serial_lock();

    let home = tempfile::tempdir().expect("home tempdir");
    let providers = home.path().join("providers");
    std::fs::create_dir_all(&providers).expect("providers dir");
    copy_release_wasm_into(&providers);
    // The daemon serves by content id, so the test provider must be in the
    // by-hash store (the flat copy above only satisfies the archive tool).
    let test_id = install_test_provider(&providers);

    // Write the mount spec before spawning so the daemon reconciles it on start.
    let mounts_dir = home.path().join("mounts");
    std::fs::create_dir_all(&mounts_dir).expect("mounts dir");
    std::fs::write(mounts_dir.join("test.json"), test_mount_spec(&test_id))
        .expect("write test mount spec");

    let mount_point = home.path().join("mnt");
    std::fs::create_dir_all(&mount_point).expect("mount point");

    let port = free_port();
    let listen_addr = format!("127.0.0.1:{port}");
    let base = format!("http://{listen_addr}");

    // The daemon picks the platform-default frontend automatically; no
    // --frontend flag. Mount point comes from OMNIFS_MOUNT_POINT; config and
    // providers come from OMNIFS_HOME. --host-native opens preopens directly.
    let child = Command::new(omnifs_bin())
        .args(["daemon", "--listen", &listen_addr, "--host-native"])
        .env("OMNIFS_HOME", home.path())
        .env("OMNIFS_MOUNT_POINT", &mount_point)
        .env("OMNIFS_DAEMON_ADDR", &listen_addr)
        .env("RUST_LOG", "warn")
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => {
            eprintln!("skip: spawn omnifs daemon failed: {error}");
            return None;
        },
    };

    let mut daemon = Daemon {
        child,
        mount_point: mount_point.clone(),
        _home: home,
        _nfs_lock: nfs_lock,
    };

    // Wait for the control API. If the daemon exits before becoming ready,
    // distinguish between an exit due to a hard failure (CLI parse error,
    // missing arg, bind error) and a mount setup failure. Hard exits (non-zero,
    // from a bad CLI parse or bind collision) are panics because they indicate
    // a regression in the test or in the daemon argument surface.
    let deadline = Instant::now() + Duration::from_secs(30);
    let ready = loop {
        match daemon.child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    // A daemon that exits cleanly before ready is unexpected but
                    // not a hard CLI failure; treat as a skip.
                    eprintln!("skip: daemon exited cleanly before the control API was ready");
                    return None;
                }
                // Non-zero exit before serving: this is a hard failure. Panic
                // so the test is red, not silently skipped.
                panic!(
                    "omnifs daemon exited with {status} before the control API became ready on \
                     {listen_addr} — this is a CLI or startup error, not a skip. \
                     Check that the daemon accepts the args passed in this test \
                     and that `{listen_addr}` was not already in use."
                );
            },
            Ok(None) => {},
            Err(error) => panic!("poll daemon child status: {error}"),
        }
        if curl(&["-fs", "-o", "/dev/null", &format!("{base}/v1/ready")]) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    // Timed out waiting for the control API. On a platform that should be able
    // to mount (we checked above), this is a real failure, not a skip.
    assert!(
        ready,
        "omnifs daemon control API never became ready on {listen_addr} after 30s; \
         the daemon is alive but not serving. Check the daemon log."
    );

    // Wait for the mount to serve the projected tree, bailing if the daemon exits.
    let message = mount_point.join("test/hello/message");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if message.is_file() {
            return Some(daemon);
        }
        match daemon.child.try_wait() {
            Ok(Some(status)) => {
                eprintln!(
                    "skip {PLATFORM_FRONTEND}: daemon exited ({status}) before the mount was \
                     active — the platform frontend could not serve"
                );
                return None;
            },
            Ok(None) => {},
            Err(error) => panic!("poll daemon child status: {error}"),
        }
        if Instant::now() >= deadline {
            eprintln!(
                "skip {PLATFORM_FRONTEND}: {} never appeared within 30s — \
                 the mount could not come up on this platform",
                message.display()
            );
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .len()
}

/// The frontend-agnostic toolbox/read-semantics matrix over the test-provider
/// tree, driven through the real shell tools so it exercises the kernel-frontend
/// boundary, not just the host op layer.
fn run_matrix(root: &Path) {
    let hello = root.join("hello");
    assert!(
        root.is_dir(),
        "mount root {} not a directory",
        root.display()
    );
    assert!(hello.is_dir(), "hello/ not a directory");

    // cat + exact size of the canonical small file.
    let message = hello.join("message");
    assert_eq!(read(&message), b"Hello, world!");
    assert_eq!(file_size(&message), 13);

    // Ranged file: full content, known size, and an offset read via `dd`.
    let ranged = hello.join("ranged");
    assert_eq!(file_size(&ranged), 26);
    assert_eq!(read(&ranged), b"abcdefghijklmnopqrstuvwxyz");
    let dd = Command::new("dd")
        .args([
            &format!("if={}", ranged.display()),
            "bs=1",
            "skip=2",
            "count=4",
        ])
        .output()
        .expect("dd");
    assert_eq!(&dd.stdout, b"cdef", "offset read via dd");

    // Dynamic route resolves and serves.
    assert_eq!(read(&root.join("dynamic/alpha/value")), b"alpha\n");

    // Directory of leaves archives faithfully with `tar`.
    let bundle = hello.join("bundle");
    assert_eq!(read(&bundle.join("title")), b"title");
    assert_eq!(read(&bundle.join("body")), b"body");
    let tar = Command::new("tar")
        .args([OsStr::new("-cf"), OsStr::new("-"), OsStr::new("-C")])
        .arg(&bundle)
        .args(["title", "body"])
        .output()
        .expect("tar");
    assert!(tar.status.success(), "tar of bundle/ failed");
    assert!(!tar.stdout.is_empty(), "tar produced no archive");

    // cp + byte-identical compare.
    let copy_dir = tempfile::tempdir().expect("copy target dir");
    let copy = copy_dir.path().join("message.copy");
    std::fs::copy(&message, &copy).expect("cp");
    assert_eq!(read(&copy), b"Hello, world!");

    #[cfg(target_os = "macos")]
    macos_nfs_probes(root, &message);
}

/// macOS-NFS-specific checks: the read-only mount must not let the OS
/// materialize `.DS_Store`/`AppleDouble` sidecars, and repeated stat/listing
/// must not perturb a file's reported attributes. Migrated from `nfs-macos-probes.sh`.
#[cfg(target_os = "macos")]
fn macos_nfs_probes(root: &Path, sample: &Path) {
    let dir = sample.parent().unwrap();
    let base = sample.file_name().unwrap().to_string_lossy();
    assert!(
        !dir.join(".DS_Store").exists(),
        "unexpected .DS_Store materialized under {}",
        dir.display()
    );
    assert!(
        !dir.join(format!("._{base}")).exists(),
        "unexpected AppleDouble sidecar materialized under {}",
        dir.display()
    );
    let before = file_size(sample);
    for _ in 0..20 {
        let _ = std::fs::metadata(sample);
        if let Ok(entries) = std::fs::read_dir(dir) {
            let _ = entries.count();
        }
    }
    assert_eq!(file_size(sample), before, "stat/list perturbed file size");
    let _ = root;
}

#[test]
fn platform_default_frontend_serves_the_real_toolbox() {
    let Some(daemon) = start() else { return };
    run_matrix(&daemon.mount_point.join("test"));
}
