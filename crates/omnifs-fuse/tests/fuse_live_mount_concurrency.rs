//! Live-kernel FUSE concurrency net: one provider read parked on a slow
//! upstream must not block other operations on the same mount.
//!
//! Twin of `crates/omnifs-nfs/tests/nfs_live_mount_concurrency.rs`, against a
//! real kernel FUSE mount on Linux. The slow upstream is played by the host's
//! callout-capture harness: the test provider's `slow/{ms}` read suspends on a
//! real async WIT fetch import and this test holds the answer for the
//! requested delay. No network is involved and no extra provider authority is
//! granted.
//!
//! The fast batch must complete while the slow read remains parked, proving
//! async dispatch prevents head-of-line blocking.

use omnifs_engine::GitCloner;
use omnifs_engine::HostContext;
use omnifs_engine::MountRuntimes;
use omnifs_fuse::new_notifier_handle;
use omnifs_wit::provider::types::{CalloutResult, Header, HttpResponse};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the slow read stays parked in the provider.
const SLOW_MS: u64 = 5000;

/// Upper bound for the fast stat+cat batch issued while the slow read is
/// parked. Serial dispatch busts this budget by seconds, not milliseconds.
const FAST_BUDGET: Duration = Duration::from_secs(2);

/// Body the "upstream" answers after the delay.
const SLOW_BODY: &[u8] = b"slow-upstream-body";

#[test]
#[allow(clippy::too_many_lines)] // one mount lifecycle: park, race, teardown
fn fuse_live_mount_serves_fast_ops_while_provider_read_is_parked() {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live-mount acceptance tests");
        return;
    }
    if !Path::new("/dev/fuse").exists() {
        eprintln!("skip: /dev/fuse unavailable; platform cannot FUSE-mount");
        return;
    }
    let wasm = provider_wasm_path("test_provider.wasm");
    if !wasm.exists() {
        eprintln!("skip: test_provider.wasm missing (run `just build providers`)");
        return;
    }

    let home = tempfile::tempdir().expect("home dir");
    let fixture = MountFixture::new(home.path(), &wasm);

    let runtime = fixture.runtime.clone();
    let mount_point = fixture.mount_point.clone();

    let mount_thread = std::thread::spawn({
        let mount_point = mount_point.clone();
        let registry = Arc::clone(&fixture.registry);
        let handle = fixture.rt.handle().clone();
        move || {
            let notifier = new_notifier_handle();
            // The daemon owns namespace construction; the live test mirrors it.
            let namespace = omnifs_engine::TreeNamespace::new(registry, handle.clone());
            omnifs_fuse::mount::run_blocking(&mount_point, namespace, &handle, &notifier)
        }
    });

    // Wait for the kernel mount to serve the projected tree; treat "cannot
    // mount here" as a skip, matching the other live acceptance tests.
    let test_root = mount_point.join("test");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if test_root.exists() {
            break;
        }
        if mount_thread.is_finished() {
            let result = mount_thread.join().expect("mount thread panicked");
            eprintln!("skip: FUSE mount did not establish: {result:?}");
            fixture.registry.shutdown_all();
            return;
        }
        if Instant::now() >= deadline {
            eprintln!("skip: FUSE mount never became active within 30s");
            force_unmount(&mount_point);
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

    // Answer after SLOW_MS, then keep answering any duplicate dispatches
    // immediately so teardown never hangs on an unanswered retry.
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
    // read parked: a cold stat+cat (real provider round trips) and a warm cat.
    let fast_started = Instant::now();
    let greeting_path = mount_point.join("test/hello/greeting");
    let metadata = std::fs::metadata(&greeting_path).expect("stat hello/greeting");
    assert!(metadata.is_file());
    assert_eq!(
        std::fs::read(&greeting_path).expect("read hello/greeting"),
        b"Hi there!\n"
    );
    assert_eq!(
        std::fs::read(&message_path).expect("re-read hello/message"),
        b"Hello, world!"
    );
    let fast_elapsed = fast_started.elapsed();
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

    // Deliberate teardown: graceful unmount, then let the session loop exit.
    graceful_unmount(&mount_point);
    drop(guard);
    mount_thread
        .join()
        .expect("mount thread panicked")
        .expect("FUSE session exits cleanly after unmount");
    fixture.registry.shutdown_all();
}

/// The registry, tokio runtime, and on-disk layout backing one live mount of
/// the test provider with captured callouts.
struct MountFixture {
    registry: Arc<MountRuntimes>,
    runtime: Arc<omnifs_engine::Engine>,
    rt: tokio::runtime::Runtime,
    mount_point: PathBuf,
}

impl MountFixture {
    fn new(home: &Path, wasm: &Path) -> Self {
        let cache_dir = home.join("cache");
        let config_dir = home.join("config");
        let providers_dir = home.join("providers");
        let mount_point = home.join("mnt");
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
        let mounts_dir = home.join("mounts");
        std::fs::create_dir_all(&mounts_dir).expect("mounts dir");
        std::fs::write(mounts_dir.join("test.json"), mount_config.as_bytes())
            .expect("write mount spec");
        let desired =
            omnifs_workspace::mounts::Registry::load(&mounts_dir).expect("load mount snapshot");

        let cloner = Arc::new(GitCloner::new(cache_dir.join("clones")));
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let registry = Arc::new(
            omnifs_engine::test_support::load_mount_runtimes_for_callout_tests(
                HostContext::new(
                    &cache_dir,
                    &config_dir,
                    &providers_dir,
                    config_dir.join("credentials.json"),
                ),
                cloner,
                &desired,
                rt.handle(),
            )
            .expect("load test mount with captured callouts"),
        );
        let runtime = registry.get("test").expect("load test mount runtime");

        Self {
            registry,
            runtime,
            rt,
            mount_point,
        }
    }
}

/// Best-effort teardown so a panicking test never wedges the mount point.
struct UnmountGuard {
    mount_point: PathBuf,
}

impl Drop for UnmountGuard {
    fn drop(&mut self) {
        force_unmount(&self.mount_point);
    }
}

fn graceful_unmount(mount_point: &Path) {
    for _ in 0..10 {
        if omnifs_fuse::mount::unmount(mount_point).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    force_unmount(mount_point);
}

/// Lazy-then-forced unmount, mirroring the acceptance fixtures' cleanup.
fn force_unmount(mount_point: &Path) {
    let _ = std::process::Command::new("fusermount")
        .arg("-uz")
        .arg(mount_point)
        .output();
    let _ = std::process::Command::new("umount")
        .arg(mount_point)
        .output();
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
