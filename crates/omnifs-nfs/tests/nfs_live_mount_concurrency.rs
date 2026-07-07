//! Live-kernel NFS concurrency net: one provider read parked on a slow
//! upstream must not block other operations on the same mount.
//!
//! The mount is a real kernel NFS mount (`mount_nfs` via `sudo -n` on macOS),
//! not an in-process export. The slow upstream is played by the host's
//! callout-capture harness: the test provider's `slow/{ms}` read suspends on a
//! real async WIT fetch import and this test holds the answer for the
//! requested delay. No network is involved and no extra provider authority is
//! granted.
//!
//! Gating follows the live-acceptance idiom (see
//! `crates/omnifs-cli/tests/lifecycle_acceptance.rs`): without
//! `OMNIFS_ACCEPTANCE_LIVE=1`, or when the platform cannot mount, the test
//! skips rather than fails, so default lanes stay green while live hosts prove
//! the real thing. Never interrupt a running live NFS test: an orphaned kernel
//! mount wedges later runs.

use omnifs_engine::GitCloner;
use omnifs_engine::HostContext;
use omnifs_engine::MountRuntimes;
use omnifs_engine::TreeNamespace;
use omnifs_nfs::{NfsMountOptions, mount_blocking, mount_is_active, unmount};
use omnifs_wit::provider::types::{CalloutResult, Header, HttpResponse};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the slow read stays parked in the provider. The fast batch must
/// complete well inside this window for the overlap assertions to mean
/// anything.
const SLOW_MS: u64 = 5000;

/// Upper bound for the fast stat+cat batch issued while the slow read is
/// parked. A frontend that head-of-line blocks behind the parked read busts
/// this budget by seconds, not milliseconds.
const FAST_BUDGET: Duration = Duration::from_secs(2);

/// Body the "upstream" answers after the delay. The slow file must serve
/// exactly these bytes, proving the read stayed in flight end to end rather
/// than erroring out and being retried.
const SLOW_BODY: &[u8] = b"slow-upstream-body";

/// Fixed, non-ephemeral port used purely as a cross-process lock for live NFS
/// mounts. Must stay identical to `NFS_LOCK_PORT` in
/// `crates/omnifs-cli/tests/common/mod.rs` so live NFS tests across test
/// binaries serialize against each other; nextest runs each integration-test
/// binary as its own process, so an in-process mutex cannot do it.
const NFS_LOCK_PORT: u16 = 48761;

fn nfs_serial_lock() -> TcpListener {
    loop {
        match TcpListener::bind(("127.0.0.1", NFS_LOCK_PORT)) {
            Ok(listener) => return listener,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)] // one mount lifecycle: park, race, teardown
fn nfs_live_mount_serves_fast_ops_while_provider_read_is_parked() {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live-mount acceptance tests");
        return;
    }
    let wasm = provider_wasm_path("test_provider.wasm");
    if !wasm.exists() {
        eprintln!("skip: test_provider.wasm missing (run `just providers build`)");
        return;
    }

    // Serialize live kernel NFS mounts across processes; held until the mount
    // is torn down.
    let _lock = nfs_serial_lock();

    let home = tempfile::tempdir().expect("home dir");
    let fixture = MountFixture::new(home.path(), &wasm);

    // The runtime handle for answering captured callouts.
    let runtime = fixture.runtime.clone();
    let mount_point = fixture.mount_point.clone();

    let mount_thread = std::thread::spawn({
        let mount_point = mount_point.clone();
        let registry = Arc::clone(&fixture.registry);
        let handle = fixture.rt.handle().clone();
        let options = fixture.options.clone();
        move || {
            let namespace = TreeNamespace::new(registry, handle.clone());
            mount_blocking(&mount_point, namespace, handle, &options, None)
        }
    });

    // Wait for the kernel mount; treat "cannot mount here" as a skip, matching
    // the other live acceptance tests.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if mount_is_active(&mount_point) {
            break;
        }
        if mount_thread.is_finished() {
            let result = mount_thread.join().expect("mount thread panicked");
            eprintln!("skip: NFS loopback mount did not establish: {result:?}");
            fixture.registry.shutdown_all();
            return;
        }
        if Instant::now() >= deadline {
            eprintln!("skip: NFS mount never became active within 30s");
            let _ = unmount(&mount_point);
            let _ = mount_thread.join();
            fixture.registry.shutdown_all();
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let guard = UnmountGuard {
        mount_point: mount_point.clone(),
    };

    // Warm-up: prove the mount serves, and warm `hello/message` so the fast
    // batch below covers one warm path and one cold path.
    let message_path = mount_point.join("test/hello/message");
    assert_eq!(
        std::fs::read(&message_path).expect("read hello/message"),
        b"Hello, world!"
    );

    // Park a read of `slow/{SLOW_MS}` in the provider.
    let slow_path = mount_point.join(format!("test/slow/{SLOW_MS}"));
    let slow_done = Arc::new(AtomicBool::new(false));
    let slow_thread = std::thread::spawn({
        let slow_done = Arc::clone(&slow_done);
        move || {
            let started = Instant::now();
            let bytes = std::fs::read(&slow_path);
            let elapsed = started.elapsed();
            slow_done.store(true, Ordering::SeqCst);
            (bytes, elapsed)
        }
    });

    // The read is genuinely parked once its fetch callout is captured.
    let captured = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(callout) = runtime.try_recv_test_callout() {
                break callout;
            }
            assert!(
                !slow_thread.is_finished(),
                "slow read finished without reaching its provider callout"
            );
            assert!(
                Instant::now() < deadline,
                "slow read never reached its provider callout"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    assert!(!slow_done.load(Ordering::SeqCst));

    // Answer after SLOW_MS, then keep answering any client-retransmit
    // duplicates immediately so teardown never hangs on an unanswered retry.
    let stop_answering = Arc::new(AtomicBool::new(false));
    let answer_thread = std::thread::spawn({
        let stop = Arc::clone(&stop_answering);
        let runtime = Arc::clone(&runtime);
        move || {
            std::thread::sleep(Duration::from_millis(SLOW_MS));
            captured.answer(http_ok(SLOW_BODY));
            while !stop.load(Ordering::SeqCst) {
                if let Some(extra) = runtime.try_recv_test_callout() {
                    extra.answer(http_ok(SLOW_BODY));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });

    // Fast operations on the same mount, issued immediately after the slow
    // read parked (the answer is scheduled SLOW_MS out, so the overlap is
    // structural): a cold stat+cat (`hello/greeting`, real provider round
    // trips) and a warm cat.
    // The fast ops run as real `stat` and `cat` processes, the toolbox the
    // product contract names. Process separation also matters for protocol
    // fidelity: NFSv4.0 serializes OPENs per open-owner and the client derives
    // open-owners per process, so an in-process `fs::read` here would share
    // the parked slow OPEN's owner and be withheld by the client itself, which
    // is client protocol state, not frontend behavior.
    let fast_started = Instant::now();
    let greeting_path = mount_point.join("test/hello/greeting");
    let stat_out = std::process::Command::new("stat")
        .arg(&greeting_path)
        .output()
        .expect("spawn stat hello/greeting");
    let stat_elapsed = fast_started.elapsed();
    assert!(stat_out.status.success(), "stat hello/greeting failed");
    let cat_greeting = std::process::Command::new("cat")
        .arg(&greeting_path)
        .output()
        .expect("spawn cat hello/greeting");
    let cold_elapsed = fast_started.elapsed();
    assert!(cat_greeting.status.success(), "cat hello/greeting failed");
    assert_eq!(cat_greeting.stdout, b"Hi there!\n");
    let cat_message = std::process::Command::new("cat")
        .arg(&message_path)
        .output()
        .expect("spawn cat hello/message");
    assert!(cat_message.status.success(), "cat hello/message failed");
    assert_eq!(cat_message.stdout, b"Hello, world!");
    let fast_elapsed = fast_started.elapsed();
    eprintln!(
        "fast ops: stat greeting {stat_elapsed:?}, +cat greeting {cold_elapsed:?}, \
         +cat message {fast_elapsed:?}"
    );
    assert!(
        fast_elapsed < FAST_BUDGET,
        "fast ops took {fast_elapsed:?} while a slow provider read was parked; \
         the mount is head-of-line blocked"
    );
    assert!(
        !slow_done.load(Ordering::SeqCst),
        "slow read completed within {fast_elapsed:?}; it no longer overlaps the fast ops"
    );

    // The parked read completes with the answered upstream body.
    let (slow_bytes, slow_elapsed) = slow_thread.join().expect("slow thread panicked");
    assert_eq!(slow_bytes.expect("slow read returns"), SLOW_BODY);
    assert!(
        slow_elapsed >= Duration::from_millis(SLOW_MS - 500),
        "slow read returned in {slow_elapsed:?}, before its upstream answered"
    );

    stop_answering.store(true, Ordering::SeqCst);
    answer_thread.join().expect("answer thread panicked");

    // Deliberate teardown: graceful unmount (with retries for a tardy NFS
    // client), then let the server loop observe the unmount and exit.
    graceful_unmount(&mount_point);
    drop(guard);
    mount_thread
        .join()
        .expect("mount thread panicked")
        .expect("mount_blocking exits cleanly after unmount");
    fixture.registry.shutdown_all();
}

/// The registry, tokio runtime, and on-disk layout backing one live mount of
/// the test provider with captured callouts.
struct MountFixture {
    registry: Arc<MountRuntimes>,
    runtime: Arc<omnifs_engine::Engine>,
    rt: tokio::runtime::Runtime,
    mount_point: PathBuf,
    options: NfsMountOptions,
}

impl MountFixture {
    fn new(home: &Path, wasm: &Path) -> Self {
        let cache_dir = home.join("cache");
        let config_dir = home.join("config");
        let providers_dir = home.join("providers");
        let mount_point = home.join("mnt");
        let state_dir = home.join("nfs-state");
        for dir in [&cache_dir, &config_dir, &providers_dir, &mount_point] {
            std::fs::create_dir_all(dir).expect("fixture dir");
        }

        let bytes = std::fs::read(wasm).expect("read test provider");
        let id = omnifs_workspace::ids::ProviderId::from_wasm_bytes(&bytes);
        let store = omnifs_workspace::provider::ProviderStore::new(&providers_dir);
        store.put_if_absent(&id, &bytes).expect("put test provider");
        store
            .install(
                id,
                omnifs_workspace::ids::ProviderMeta {
                    name: omnifs_workspace::ids::ProviderName::new("test-provider").unwrap(),
                    version: None,
                },
                "test_provider.wasm".into(),
            )
            .expect("install test provider");

        let mount_config = format!(
            r#"{{
                "provider": {{ "id": "{id}", "meta": {{ "name": "test-provider" }} }},
                "mount": "test",
                "capabilities": {{ "domains": ["httpbin.org"] }}
            }}"#
        );
        let spec = omnifs_workspace::mounts::Spec::parse(&mount_config).expect("parse mount spec");

        let cloner = Arc::new(GitCloner::new(cache_dir.join("clones")));
        let registry = Arc::new(
            MountRuntimes::new(
                HostContext::new(
                    &cache_dir,
                    &config_dir,
                    &providers_dir,
                    config_dir.join("credentials.json"),
                ),
                cloner,
            )
            .expect("registry init"),
        );

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let runtime = registry
            .add_mount_for_callout_tests(&spec, rt.handle())
            .expect("add test mount with captured callouts");

        let mut options = NfsMountOptions::loopback(state_dir);
        if let Some(trace) = std::env::var_os("OMNIFS_NFS_TEST_TRACE") {
            options.trace_path = Some(PathBuf::from(trace));
        }

        Self {
            registry,
            runtime,
            rt,
            mount_point,
            options,
        }
    }
}

/// Best-effort teardown so a panicking test never wedges later live runs with
/// an orphaned kernel mount.
struct UnmountGuard {
    mount_point: PathBuf,
}

impl Drop for UnmountGuard {
    fn drop(&mut self) {
        if mount_is_active(&self.mount_point) {
            force_unmount(&self.mount_point);
        }
    }
}

fn graceful_unmount(mount_point: &Path) {
    for _ in 0..10 {
        if !mount_is_active(mount_point) {
            return;
        }
        if unmount(mount_point).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    force_unmount(mount_point);
}

/// Mirror of production teardown for a wedged mount: `sudo -n umount -f`
/// clears a dead-server NFS mount where `diskutil unmount` would block.
fn force_unmount(mount_point: &Path) {
    #[cfg(target_os = "macos")]
    let output = std::process::Command::new("sudo")
        .args(["-n", "umount", "-f"])
        .arg(mount_point)
        .output();
    #[cfg(not(target_os = "macos"))]
    let output = std::process::Command::new("umount")
        .arg("-f")
        .arg(mount_point)
        .output();
    let _ = output; // best-effort: the guard must never panic in Drop
}

fn http_ok(body: &[u8]) -> CalloutResult {
    CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::<Header>::new(),
        body: body.to_vec(),
    })
}

fn provider_wasm_path(file_name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join(file_name)
}
