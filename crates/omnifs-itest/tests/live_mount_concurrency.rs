//! Live-kernel concurrency acceptance for both filesystem frontends.
//!
//! One provider read waits on a captured host callout while independent reads
//! and metadata operations use the same mount. The fast work must finish before
//! the captured callout is answered, proving that neither frontend serializes
//! unrelated namespace work behind one suspended provider operation.

use omnifs_engine::{MountTable, TreeNamespace};
use omnifs_wit::provider::types::{CalloutResult, Header, HttpResponse};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SLOW_MS: u64 = 5000;
const FAST_BUDGET: Duration = Duration::from_secs(2);
const SLOW_BODY: &[u8] = b"slow-upstream-body";

#[derive(Clone, Copy)]
enum Frontend {
    Fuse,
    Nfs,
}

impl Frontend {
    fn name(self) -> &'static str {
        match self {
            Self::Fuse => "FUSE",
            Self::Nfs => "NFS",
        }
    }

    fn supported(self) -> bool {
        match self {
            Self::Fuse => Path::new("/dev/fuse").exists(),
            Self::Nfs => true,
        }
    }

    fn mount(
        self,
        home: &Path,
        mount_point: &Path,
        registry: Arc<MountTable>,
        handle: tokio::runtime::Handle,
    ) -> Result<(), String> {
        let namespace = TreeNamespace::online(registry, handle.clone());
        match self {
            Self::Fuse => {
                let notifier = omnifs_fuse::new_notifier_handle();
                omnifs_fuse::mount::run_blocking(mount_point, namespace, &handle, &notifier)
                    .map_err(|error| error.to_string())
            },
            Self::Nfs => {
                let mut options = omnifs_nfs::NfsMountOptions::loopback(home.join("nfs-state"));
                if let Some(trace) = std::env::var_os("OMNIFS_NFS_TEST_TRACE") {
                    options.trace_path = Some(PathBuf::from(trace));
                }
                omnifs_nfs::mount_blocking(mount_point, namespace, handle, &options)
                    .map_err(|error| error.to_string())
            },
        }
    }

    fn is_active(self, mount_point: &Path) -> bool {
        match self {
            Self::Fuse => mount_point.join("test").exists(),
            Self::Nfs => omnifs_nfs::mount_is_active(mount_point),
        }
    }

    fn fast_ops(self, mount_point: &Path, message_path: &Path) {
        let greeting_path = mount_point.join("test/hello/greeting");
        match self {
            Self::Fuse => {
                assert!(
                    std::fs::metadata(&greeting_path)
                        .expect("stat hello/greeting")
                        .is_file()
                );
                assert_eq!(
                    std::fs::read(&greeting_path).expect("read hello/greeting"),
                    b"Hi there!\n"
                );
                assert_eq!(
                    std::fs::read(message_path).expect("re-read hello/message"),
                    b"Hello, world!"
                );
            },
            Self::Nfs => {
                // Separate processes get separate NFSv4.0 open owners, so this
                // measures frontend concurrency rather than client OPEN ordering.
                let stat = std::process::Command::new("stat")
                    .arg(&greeting_path)
                    .output()
                    .expect("spawn stat hello/greeting");
                assert!(stat.status.success(), "stat hello/greeting failed");
                let greeting = std::process::Command::new("cat")
                    .arg(&greeting_path)
                    .output()
                    .expect("spawn cat hello/greeting");
                assert!(greeting.status.success(), "cat hello/greeting failed");
                assert_eq!(greeting.stdout, b"Hi there!\n");
                let message = std::process::Command::new("cat")
                    .arg(message_path)
                    .output()
                    .expect("spawn cat hello/message");
                assert!(message.status.success(), "cat hello/message failed");
                assert_eq!(message.stdout, b"Hello, world!");
            },
        }
    }

    fn graceful_unmount(self, mount_point: &Path) {
        for _ in 0..10 {
            match self {
                Self::Fuse if omnifs_fuse::mount::unmount(mount_point).is_ok() => return,
                Self::Nfs if !self.is_active(mount_point) => return,
                Self::Nfs if omnifs_nfs::unmount(mount_point).is_ok() => return,
                Self::Fuse | Self::Nfs => {},
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        self.force_unmount(mount_point);
    }

    fn force_unmount(self, mount_point: &Path) {
        match self {
            Self::Fuse => {
                let _ = std::process::Command::new("fusermount")
                    .arg("-uz")
                    .arg(mount_point)
                    .output();
                let _ = std::process::Command::new("umount")
                    .arg(mount_point)
                    .output();
            },
            Self::Nfs => {
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
                let _ = output;
            },
        }
    }
}

#[test]
fn fuse_serves_fast_ops_while_provider_read_is_parked() {
    run_concurrency(Frontend::Fuse);
}

#[test]
fn nfs_serves_fast_ops_while_provider_read_is_parked() {
    run_concurrency(Frontend::Nfs);
}

#[allow(clippy::too_many_lines)]
fn run_concurrency(frontend: Frontend) {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live-mount acceptance tests");
        return;
    }
    if !frontend.supported() {
        eprintln!("skip: {} is unavailable on this host", frontend.name());
        return;
    }
    let wasm = provider_wasm_path("test_provider.wasm");
    if !wasm.exists() {
        eprintln!("skip: test_provider.wasm missing (run `just build providers`)");
        return;
    }

    let _nfs_lock = matches!(frontend, Frontend::Nfs).then(omnifs_itest::live::nfs_serial_lock);
    let home = tempfile::tempdir().expect("home dir");
    let fixture = MountFixture::new(home.path(), &wasm);
    let runtime = Arc::clone(&fixture.runtime);
    let mount_point = fixture.mount_point.clone();
    let mount_thread = std::thread::spawn({
        let home = home.path().to_path_buf();
        let mount_point = mount_point.clone();
        let registry = Arc::clone(&fixture.registry);
        let handle = fixture.rt.handle().clone();
        move || frontend.mount(&home, &mount_point, registry, handle)
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if frontend.is_active(&mount_point) {
            break;
        }
        if mount_thread.is_finished() {
            let result = mount_thread.join().expect("mount thread panicked");
            eprintln!(
                "skip: {} mount did not establish: {result:?}",
                frontend.name()
            );
            fixture.registry.shutdown_all();
            return;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "skip: {} mount never became active within 30s",
                frontend.name()
            );
            frontend.force_unmount(&mount_point);
            let _ = mount_thread.join();
            fixture.registry.shutdown_all();
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let guard = UnmountGuard {
        frontend,
        mount_point: mount_point.clone(),
    };

    let message_path = mount_point.join("test/hello/message");
    assert_eq!(
        std::fs::read(&message_path).expect("read hello/message"),
        b"Hello, world!"
    );

    let slow_done = Arc::new(AtomicBool::new(false));
    let slow_thread = std::thread::spawn({
        let slow_done = Arc::clone(&slow_done);
        let slow_path = mount_point.join(format!("test/slow/{SLOW_MS}"));
        move || {
            let started = Instant::now();
            let bytes = std::fs::read(&slow_path);
            let elapsed = started.elapsed();
            slow_done.store(true, Ordering::SeqCst);
            (bytes, elapsed)
        }
    });

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

    let fast_started = Instant::now();
    frontend.fast_ops(&mount_point, &message_path);
    let fast_elapsed = fast_started.elapsed();
    assert!(
        fast_elapsed < FAST_BUDGET,
        "{} fast ops took {fast_elapsed:?} while a slow read was parked",
        frontend.name()
    );
    assert!(
        !slow_done.load(Ordering::SeqCst),
        "slow read completed within {fast_elapsed:?}; it no longer overlaps the fast ops"
    );

    let (slow_bytes, slow_elapsed) = slow_thread.join().expect("slow thread panicked");
    assert_eq!(slow_bytes.expect("slow read returns"), SLOW_BODY);
    assert!(
        slow_elapsed >= Duration::from_millis(SLOW_MS - 500),
        "slow read returned before its upstream answer: {slow_elapsed:?}"
    );
    stop_answering.store(true, Ordering::SeqCst);
    answer_thread.join().expect("answer thread panicked");

    frontend.graceful_unmount(&mount_point);
    drop(guard);
    mount_thread
        .join()
        .expect("mount thread panicked")
        .expect("frontend exits cleanly after unmount");
    fixture.registry.shutdown_all();
}

struct MountFixture {
    registry: Arc<MountTable>,
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

        let artifact =
            omnifs_workspace::provider::Artifact::from_file(wasm).expect("parse test provider");
        let id = artifact.id();
        omnifs_workspace::provider::ProviderStore::new(&providers_dir)
            .retain(&artifact)
            .expect("retain test provider");
        let mounts_dir = home.join("mounts");
        std::fs::create_dir_all(&mounts_dir).expect("mounts dir");
        std::fs::write(
            mounts_dir.join("test.json"),
            format!(
                r#"{{"provider":{{"id":"{id}","meta":{{"name":"test-provider"}}}},"mount":"test"}}"#
            ),
        )
        .expect("write mount spec");
        let desired =
            omnifs_workspace::mounts::Registry::load(&mounts_dir).expect("load mount snapshot");
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let host = omnifs_engine::test_support::open_test_host(
            &cache_dir,
            &providers_dir,
            config_dir.join("credentials.json"),
            cache_dir.join("clones"),
        )
        .expect("open test host");
        let registry = Arc::new(
            omnifs_engine::test_support::load_mount_table_for_callout_tests(
                host.as_online().expect("online host"),
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

struct UnmountGuard {
    frontend: Frontend,
    mount_point: PathBuf,
}

impl Drop for UnmountGuard {
    fn drop(&mut self) {
        if self.frontend.is_active(&self.mount_point) {
            self.frontend.force_unmount(&self.mount_point);
        }
    }
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
        .join("target/wasm32-wasip2/release")
        .join(file_name)
}
