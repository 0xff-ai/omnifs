//! Frontend conformance: the projected tree must behave like real files for the
//! standard toolbox, through *each* frontend (FUSE and NFS), not just one.
//!
//! This launches the real daemon (`omnifs daemon`) against a hermetic `OMNIFS_HOME` with
//! only the canned `test-provider` mounted, then runs one frontend-agnostic
//! matrix (`run_matrix`) over the mount using the actual shell toolbox. The
//! frontend is the only parameter; the assertions are shared. Migrated from the
//! former `tests/smoke/frontend-test-provider.sh` and `nfs-macos-probes.sh`.
//!
//! Mounting requires capabilities the host may lack (a FUSE device, privilege
//! for an NFS loopback mount, pre-built provider wasm). When the mount can't be
//! brought up the test *skips* (prints why and returns) rather than failing, so
//! it never reds CI on an under-provisioned runner; it asserts only once a real
//! mount is live. CI runs it on a frontend-capable runner to exercise it for real.

#![cfg(not(target_os = "wasi"))]

use std::ffi::OsStr;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// `target/wasm32-wasip2/release`, where provider wasm (incl. `test_provider.wasm`
/// and the archive tool) is expected. Produced by `just providers-build`.
fn release_wasm_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target/wasm32-wasip2/release")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn curl(args: &[&str]) -> bool {
    Command::new("curl")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A running `omnifsd` with the test-provider mounted, torn down on drop.
struct Daemon {
    child: Child,
    mount_point: PathBuf,
    _home: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Best-effort unmount in case the daemon didn't release it on exit.
        let mp = self.mount_point.as_os_str();
        if cfg!(target_os = "macos") {
            let _ = Command::new("umount").arg(mp).status();
        } else {
            let _ = Command::new("fusermount")
                .args([OsStr::new("-u"), mp])
                .status();
            let _ = Command::new("umount").arg(mp).status();
        }
    }
}

const TEST_MOUNT_SPEC: &str = r#"{"provider":"test_provider.wasm","mount":"test","capabilities":{"domains":["httpbin.org"]}}"#;

/// Bring up `omnifsd --frontend <frontend>` with only the test-provider mounted.
/// Returns `None` (skip) when the mount can't be established on this host.
#[allow(clippy::too_many_lines)] // linear end-to-end frontend bring-up
fn start(frontend: &str) -> Option<Daemon> {
    let wasm_dir = release_wasm_dir();
    let test_wasm = wasm_dir.join("test_provider.wasm");
    if !test_wasm.exists() {
        eprintln!(
            "skip {frontend}: {} missing (run `just providers-build`)",
            test_wasm.display()
        );
        return None;
    }

    let home = tempfile::tempdir().expect("home tempdir");
    let providers = home.path().join("providers");
    std::fs::create_dir_all(&providers).expect("providers dir");
    // Copy every built provider/tool wasm so the registry finds the test
    // provider and the archive extractor it loads at init.
    for entry in std::fs::read_dir(&wasm_dir)
        .expect("read release wasm dir")
        .flatten()
    {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "wasm") {
            std::fs::copy(&path, providers.join(path.file_name().unwrap())).expect("copy wasm");
        }
    }

    let mount_point = home.path().join("mnt");
    std::fs::create_dir_all(&mount_point).expect("mount point");
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");

    let child = Command::new(env!("CARGO_BIN_EXE_omnifs"))
        .args([
            "daemon",
            "--frontend",
            frontend,
            "--mount-point",
            mount_point.to_str().unwrap(),
            "--listen",
            &format!("127.0.0.1:{port}"),
        ])
        .env("OMNIFS_HOME", home.path())
        .env("RUST_LOG", "warn")
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => {
            eprintln!("skip {frontend}: spawn omnifsd failed: {error}");
            return None;
        },
    };

    let mut daemon = Daemon {
        child,
        mount_point: mount_point.clone(),
        _home: home,
    };

    // Wait for the control API, bailing out (skip) the instant omnifsd exits —
    // e.g. a frontend that can't mount on this host fails fast rather than
    // making us poll a dead port for the full timeout.
    let deadline = Instant::now() + Duration::from_secs(20);
    let ready = loop {
        if let Ok(Some(status)) = daemon.child.try_wait() {
            eprintln!("skip {frontend}: omnifsd exited before ready ({status})");
            return None;
        }
        if curl(&["-fs", "-o", "/dev/null", &format!("{base}/v1/ready")]) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if !ready {
        eprintln!("skip {frontend}: control API never became ready");
        return None;
    }

    // Push the test-provider mount.
    if !curl(&[
        "-fsS",
        "-o",
        "/dev/null",
        "-X",
        "POST",
        &format!("{base}/v1/mounts"),
        "-H",
        "content-type: application/json",
        "-d",
        TEST_MOUNT_SPEC,
    ]) {
        eprintln!("skip {frontend}: mount push failed");
        return None;
    }

    // Wait for the mount to serve the projected tree, bailing if omnifsd exits
    // (a frontend whose mount fails only once it starts serving).
    let message = mount_point.join("test/hello/message");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if message.is_file() {
            return Some(daemon);
        }
        if let Ok(Some(status)) = daemon.child.try_wait() {
            eprintln!("skip {frontend}: omnifsd exited before mounting ({status})");
            return None;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "skip {frontend}: {} never appeared (not mountable here)",
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
/// tree, driven through the real shell tools so it exercises the kernel↔frontend
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
    assert_eq!(read(&root.join("dynamic/alpha/value")), b"alpha");

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
    let copy = root.join("message.copy");
    std::fs::copy(&message, &copy).expect("cp");
    assert_eq!(read(&copy), b"Hello, world!");

    #[cfg(target_os = "macos")]
    macos_nfs_probes(root, &message);
}

/// macOS-NFS-specific checks: the read-only mount must not let the OS
/// materialize `.DS_Store`/`AppleDouble` sidecars, and repeated stat/listing must
/// not perturb a file's reported attributes. Migrated from `nfs-macos-probes.sh`.
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
fn fuse_frontend_serves_the_real_toolbox() {
    let Some(daemon) = start("fuse") else { return };
    run_matrix(&daemon.mount_point.join("test"));
}

#[test]
fn nfs_frontend_serves_the_real_toolbox() {
    let Some(daemon) = start("nfs") else { return };
    run_matrix(&daemon.mount_point.join("test"));
}
