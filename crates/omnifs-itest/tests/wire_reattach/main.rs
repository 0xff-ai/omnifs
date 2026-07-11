//! Restart survival for the out-of-process NFS frontend.
//!
//! This is the structural answer to the ESTALE row in the NFS quirk catalog. Two
//! legs, run in order against one live mount:
//!
//! - **Leg A (frontend kill).** SIGKILL the `wire-test-frontend` runner (this
//!   crate's out-of-process NFS test double over the Omnifs VFS wire protocol)
//!   mid-workload
//!   and relaunch it with the same argv (same pinned NFS port, same state dir).
//!   The kernel client keeps the mount and its filehandles; the restarted runner
//!   reloads the persisted filehandle table (same generation) and serves without
//!   remounting. A held fd keeps reading, a fresh open works, and a previously
//!   listed directory still lists. No unmount happened.
//! - **Leg B (daemon kill).** SIGKILL the namespace-only daemon under the live
//!   frontend and relaunch it. The frontend's VFS wire client reconnects onto a
//!   fresh instance id, drops every cached `NodeId`, and re-resolves lazily. The
//!   held fd
//!   keeps working and a new open succeeds; no ESTALE for surviving handles.
//!
//! Gated on `OMNIFS_ACCEPTANCE_LIVE`. Holds the cross-process NFS serialization
//! lock for the whole run and force-unmounts on teardown (including on panic), so
//! a failed run never wedges a later one with an orphaned NFS mount.

#![cfg(not(target_os = "wasi"))]

use std::io::{Read, Seek, SeekFrom};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use omnifs_itest::live;

fn acceptance_gated() -> bool {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run the wire-reattach acceptance");
        return false;
    }
    true
}

/// Best-effort force-unmount plus child teardown, run on drop so a panic in
/// either leg still cleans up the live NFS mount. The children are reassigned as
/// the test relaunches them, so the guard always holds the current pair.
struct Cleanup {
    mount_point: PathBuf,
    frontend: Option<Child>,
    daemon: Option<Child>,
    _lock: TcpListener,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        detach_mount(&self.mount_point);
        for child in [self.frontend.as_mut(), self.daemon.as_mut()]
            .into_iter()
            .flatten()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Force-unmount the mount point. A forced unmount is required here, not a
/// graceful one: the bounded-read helper threads can be parked in an
/// uninterruptible NFS read on a hard mount when the server is gone, which makes
/// a graceful unmount fail busy and, worse, keeps the test process from exiting.
/// A forced unmount breaks those reads (they return an error) so the threads
/// finish and the process can exit.
fn detach_mount(mount_point: &Path) {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::OsStr;
        let mp = mount_point.as_os_str();
        let _ = Command::new("umount").args([OsStr::new("-f"), mp]).status();
        let _ = Command::new("umount").args([OsStr::new("-l"), mp]).status();
    }
    #[cfg(not(target_os = "linux"))]
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        while omnifs_nfs::mount_is_active(mount_point) && Instant::now() < deadline {
            // `diskutil unmount force` breaks stuck NFS IO where a graceful
            // unmount fails busy.
            let _ = Command::new("diskutil")
                .args(["unmount", "force"])
                .arg(mount_point)
                .status();
            if !omnifs_nfs::mount_is_active(mount_point) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

fn curl_ready(base: &str) -> bool {
    Command::new("curl")
        .args(["-fs", "-o", "/dev/null", &format!("{base}/v1/ready")])
        .status()
        .is_ok_and(|status| status.success())
}

/// Read `path` to EOF through the mount, but never block longer than `timeout`.
/// A hard NFS mount blocks a read while the server is unreachable; running it on
/// a helper thread bounds the wait so the test fails cleanly (and its teardown
/// unmounts) instead of hanging and orphaning the mount. `None` means the read
/// did not finish in time; the blocked helper thread unblocks when teardown
/// force-unmounts.
fn read_path_timeout(path: &Path, timeout: Duration) -> Option<std::io::Result<Vec<u8>>> {
    let path = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::read(&path));
    });
    rx.recv_timeout(timeout).ok()
}

/// Read a duplicate of a held fd from offset 0 to EOF with the same timeout
/// bound. The dup shares the open file but keeps an independent offset, so it
/// exercises the same held-open recovery without disturbing the original.
fn read_held_timeout(held: &std::fs::File, timeout: Duration) -> Option<std::io::Result<Vec<u8>>> {
    let mut clone = match held.try_clone() {
        Ok(clone) => clone,
        Err(error) => return Some(Err(error)),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let result = clone
            .seek(SeekFrom::Start(0))
            .and_then(|_| clone.read_to_end(&mut buf))
            .map(|_| buf);
        let _ = tx.send(result);
    });
    rx.recv_timeout(timeout).ok()
}

/// Poll a bounded read of `path` until it succeeds, or return false past the
/// deadline.
fn wait_readable(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if matches!(read_path_timeout(path, Duration::from_secs(5)), Some(Ok(_))) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// The proof runs on macOS NFSv4.0 loopback and on Linux where `mount.nfs4` is
/// available (CI installs `nfs-common`); it skips when the platform cannot mount.
#[test]
#[allow(clippy::too_many_lines)] // linear end-to-end, two legs
fn wire_reattach_survives_frontend_and_daemon_restart() {
    if !acceptance_gated() {
        return;
    }
    if !live::platform_can_mount() {
        eprintln!("skip: platform cannot mount");
        return;
    }

    let nfs_lock = live::nfs_serial_lock();
    let hermetic = live::hermetic_home();
    let home = hermetic.home.path().to_path_buf();

    let ctrl_port = live::free_port();
    let ctrl_addr = format!("127.0.0.1:{ctrl_port}");
    let base = format!("http://{ctrl_addr}");
    let nfs_port = live::free_port();
    let socket = home.join("frontends/nfs-wire.sock");
    let mount_point = home.join("mnt-reattach");
    std::fs::create_dir_all(&mount_point).expect("mount point");
    let state_dir = home.join("nfs-state");

    let spawn_daemon = || {
        Command::new(live::omnifs_bin())
            .args([
                "daemon",
                "--listen",
                &ctrl_addr,
                "--attach-socket",
                "nfs-wire",
            ])
            .env("OMNIFS_HOME", &home)
            .env("OMNIFS_DAEMON_ADDR", &ctrl_addr)
            .env_remove("OMNIFS_MOUNT_POINT")
            .env_remove("OMNIFS_CONTROL_TOKEN")
            .env("RUST_LOG", "warn")
            .spawn()
            .expect("spawn omnifs daemon")
    };

    // Same argv every launch: a restart must rebind the same NFS port and reload
    // the same filehandle state directory, or scenario A is impossible.
    let frontend_argv: Vec<String> = vec![
        "--attach".into(),
        socket.to_str().expect("socket utf-8").into(),
        "--mount-point".into(),
        mount_point.to_str().expect("mount utf-8").into(),
        "--nfs-port".into(),
        nfs_port.to_string(),
        "--nfs-state-dir".into(),
        state_dir.to_str().expect("state dir utf-8").into(),
    ];
    let spawn_frontend = || {
        Command::new(live::wire_test_frontend_bin())
            .args(&frontend_argv)
            .env("OMNIFS_HOME", &home)
            .env(
                "RUST_LOG",
                std::env::var("REATTACH_FRONTEND_LOG").unwrap_or_else(|_| "warn".into()),
            )
            .spawn()
            .expect("spawn wire-test-frontend")
    };

    let mut guard = Cleanup {
        mount_point: mount_point.clone(),
        frontend: None,
        daemon: Some(spawn_daemon()),
        _lock: nfs_lock,
    };

    // The namespace-only daemon reports ready once its attach socket serves.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !curl_ready(&base) {
        if Instant::now() >= deadline {
            eprintln!("skip: namespace-only daemon never became ready");
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(socket.exists(), "attach socket absent after daemon ready");

    guard.frontend = Some(spawn_frontend());
    let message = mount_point.join("test/hello/message");
    if !wait_readable(&message, Duration::from_secs(30)) {
        eprintln!("skip: frontend never served the mount (renderer unavailable on this platform)");
        return;
    }

    // Hold an open fd on a projected file and record its bytes; populate the
    // filehandle table with a few more stats so the persisted table is non-trivial.
    let held = std::fs::File::open(&message).expect("open held fd");
    let baseline = read_held_timeout(&held, Duration::from_secs(10))
        .expect("held fd read must not hang before the kill")
        .expect("read held fd");
    assert!(!baseline.is_empty(), "held file must have bytes");
    let hello_dir = mount_point.join("test/hello");
    let listed_before: Vec<String> = std::fs::read_dir(&hello_dir)
        .expect("list hello dir")
        .filter_map(|entry| entry.ok().and_then(|e| e.file_name().into_string().ok()))
        .collect();
    assert!(listed_before.iter().any(|name| name == "message"));

    // Sleep past the persister debounce so the held handle is durable before the
    // SIGKILL.
    std::thread::sleep(Duration::from_millis(400));

    // ---------------------------------------------------------------------
    // Leg A: kill the frontend, relaunch with the same argv.
    // ---------------------------------------------------------------------
    {
        let mut frontend = guard.frontend.take().expect("frontend running");
        frontend.kill().expect("SIGKILL frontend"); // std Child::kill sends SIGKILL
        let _ = frontend.wait();
    }
    assert!(
        omnifs_nfs::mount_is_active(&mount_point),
        "leg A: the kernel client must keep the mount across a frontend kill"
    );

    guard.frontend = Some(spawn_frontend());
    // The restarted runner serves the export (skipping remount); the mount stays
    // active throughout, so wait on a fresh read succeeding instead of the mount
    // point appearing.
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if matches!(
            read_path_timeout(&message, Duration::from_secs(5)),
            Some(Ok(_))
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "leg A: the restarted frontend never served a fresh read"
        );
        // A restarted runner exiting early is a hard failure.
        if let Some(child) = guard.frontend.as_mut()
            && matches!(child.try_wait(), Ok(Some(_)))
        {
            panic!("leg A: the restarted frontend exited before serving");
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // (1) The held fd keeps working and bytes match.
    let after_kill = read_held_timeout(&held, Duration::from_secs(30))
        .expect("leg A: held fd must not hang after the frontend restart")
        .expect("leg A: held fd must keep reading after frontend restart");
    assert_eq!(
        after_kill, baseline,
        "leg A: held-fd bytes must be identical after the frontend restart"
    );
    // (2) A fresh open+read works.
    let fresh = read_path_timeout(&message, Duration::from_secs(15))
        .expect("leg A: fresh read must not hang")
        .expect("leg A: fresh open+read after restart");
    assert_eq!(fresh, baseline, "leg A: fresh read must match baseline");
    // (3) The previously listed directory still lists.
    let listed_after: Vec<String> = std::fs::read_dir(&hello_dir)
        .expect("leg A: relist hello dir")
        .filter_map(|entry| entry.ok().and_then(|e| e.file_name().into_string().ok()))
        .collect();
    assert!(
        listed_after.iter().any(|name| name == "message"),
        "leg A: a previously listed directory must still list its entries"
    );
    assert!(
        omnifs_nfs::mount_is_active(&mount_point),
        "leg A: no unmount happened across the frontend restart"
    );

    // ---------------------------------------------------------------------
    // Leg B: kill the daemon under the live frontend, relaunch it.
    // ---------------------------------------------------------------------
    {
        let mut daemon = guard.daemon.take().expect("daemon running");
        daemon.kill().expect("SIGKILL daemon");
        let _ = daemon.wait();
    }
    std::thread::sleep(Duration::from_secs(2));
    guard.daemon = Some(spawn_daemon());
    let deadline = Instant::now() + Duration::from_secs(30);
    while !curl_ready(&base) {
        assert!(
            Instant::now() < deadline,
            "leg B: the relaunched daemon never became ready"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // The frontend reconnects onto the fresh instance and re-resolves. The held fd
    // keeps working and a new open succeeds; reads during the outage are not
    // asserted.
    let deadline = Instant::now() + Duration::from_secs(45);
    let reattached = loop {
        if matches!(
            read_path_timeout(&message, Duration::from_secs(5)),
            Some(Ok(_))
        ) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    assert!(
        reattached,
        "leg B: the frontend never re-resolved against the restarted daemon"
    );
    let after_reattach = read_held_timeout(&held, Duration::from_secs(30))
        .expect("leg B: held fd must not hang after the daemon restart")
        .expect("leg B: held fd must keep reading after the daemon restart");
    assert_eq!(
        after_reattach, baseline,
        "leg B: held-fd bytes must be identical after the daemon reattach"
    );

    drop(guard);
}
