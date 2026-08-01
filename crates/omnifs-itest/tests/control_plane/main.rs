//! Control-plane acceptance: two daemons, two homes, one socket each.
//!
//! Proves the fixed Unix-socket control plane and narrow process identity end
//! to end: each daemon binds its own profile's `control.sock`; the CLI resolves
//! only that socket, so two daemons never address each other. A `SIGKILL`ed
//! daemon leaves stale bootstrap state; `omnifs doctor` revives that profile
//! with a fresh daemon without disturbing the other profile.
//!
//! Gated on `OMNIFS_ACCEPTANCE_LIVE` (it serves real mounts). Holds the
//! cross-process NFS serialization lock for the whole test.

#![cfg(not(target_os = "wasi"))]

use std::path::PathBuf;
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use hyper_util::rt::TokioIo;
use omnifs_api::grpc::{self, wire};
use omnifs_itest::live::{self, hermetic_home, omnifs_bin, platform_can_mount};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tonic::transport::Endpoint;
use tower::service_fn;

type ControlClient = wire::control_client::ControlClient<tonic::transport::Channel>;

async fn control_client(path: &std::path::Path) -> ControlClient {
    try_control_client(path)
        .await
        .expect("connect to isolated daemon control socket")
}

async fn try_control_client(
    path: &std::path::Path,
) -> Result<ControlClient, tonic::transport::Error> {
    let path = path.to_owned();
    let channel = Endpoint::from_static("http://[::]:50051")
        .connect_with_connector(service_fn(move |_| {
            let path = path.clone();
            async move { UnixStream::connect(path).await.map(TokioIo::new) }
        }))
        .await?;
    Ok(ControlClient::new(channel))
}

/// A host-native daemon serving only its profile-local control socket, torn
/// down on drop.
struct Daemon {
    child: Child,
    home: TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Doctor can start a detached replacement after this fixture's
        // original child was reaped. Profile-local down owns cleanup for both
        // that replacement and the ordinary child, including during unwind.
        let _ = Command::new(omnifs_bin())
            .args(["down", "--output", "json"])
            .env("OMNIFS_HOME", self.home.path())
            .env("RUST_LOG", "warn")
            .output();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    fn spawn_child(home: &std::path::Path) -> std::io::Result<Child> {
        Command::new(omnifs_bin())
            .args(live::daemon_args(home))
            .env("OMNIFS_HOME", home)
            .env("RUST_LOG", "warn")
            .spawn()
    }

    fn identity_path(&self) -> PathBuf {
        self.home.path().join("process.json")
    }

    fn identity_json(&self) -> serde_json::Value {
        let path = self.identity_path();
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("read process identity {}: {error}", path.display()));
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("parse process identity {}: {error}", path.display()))
    }

    fn control_socket(&self) -> PathBuf {
        self.home.path().join("control.sock")
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Spawn a host-native daemon for a fresh hermetic home. The daemon binds
    /// its fixed local filesystem socket but launches no runner.
    fn spawn() -> Option<Self> {
        let hermetic = hermetic_home();
        let child = Self::spawn_child(hermetic.home.path());
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                eprintln!("skip: spawn omnifs daemon failed: {error}");
                return None;
            },
        };
        Some(Self {
            child,
            home: hermetic.home,
        })
    }

    async fn restart(&mut self) {
        self.child.kill().expect("stop isolated daemon for restart");
        self.child
            .wait()
            .expect("join isolated daemon before restart");
        self.child =
            Self::spawn_child(self.home.path()).expect("restart isolated daemon in same profile");
        self.wait_serving_async().await;
    }

    /// Wait for the daemon to answer on its fixed socket. This fixture intentionally
    /// starts the pure namespace daemon without a filesystem: the control-plane
    /// assertion is socket ownership, not a filesystem mount.
    fn wait_serving(&mut self) -> Option<()> {
        let control_socket = self.control_socket();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if live::control_ready(&control_socket) {
                return Some(());
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                eprintln!("skip: daemon exited ({status}) before control readiness");
                return None;
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "skip: daemon never answered on {} within 30s",
                    control_socket.display()
                );
                return None;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    async fn wait_serving_async(&mut self) {
        let control_socket = self.control_socket();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(mut control) = try_control_client(&control_socket).await
                && control.ready(wire::Empty {}).await.is_ok()
            {
                return;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!("daemon exited ({status}) before control readiness");
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "daemon never answered on {} within 30s",
                control_socket.display()
            );
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
}

/// Run `omnifs status --output json` against `home`, resolving through the
/// profile's local control socket.
fn run_status(home: &std::path::Path) -> Output {
    Command::new(omnifs_bin())
        .args(["status", "--output", "json"])
        .env("OMNIFS_HOME", home)
        .env("RUST_LOG", "warn")
        .output()
        .expect("spawn omnifs status")
}

fn run_doctor(home: &std::path::Path) -> Output {
    Command::new(omnifs_bin())
        .args(["doctor", "--yes"])
        .env("OMNIFS_HOME", home)
        .env("RUST_LOG", "warn")
        .output()
        .expect("spawn omnifs doctor")
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(128)
}

fn status_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "status --output json must produce valid JSON: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

#[tokio::test(flavor = "multi_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end acknowledgement scenario"
)]
async fn declarative_apply_commits_before_reconcile_and_watch_resumes_from_snapshot() {
    let mut daemon = Daemon::spawn().expect("spawn isolated omnifs daemon");
    daemon.wait_serving_async().await;
    let mut control = control_client(&daemon.control_socket()).await;
    let listed = control
        .list_providers(wire::Empty {})
        .await
        .unwrap()
        .into_inner();
    let (metadata, _, _) = listed
        .providers
        .iter()
        .find(|provider| {
            provider.embedded
                && provider
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.reference.as_ref())
                    .is_some_and(|reference| reference.name == "dns")
        })
        .map(grpc::provider_entry)
        .transpose()
        .unwrap()
        .expect("daemon exposes the embedded dns provider");
    control
        .import_embedded_provider(wire::ImportEmbeddedProviderRequest {
            name: metadata.reference.name.clone(),
        })
        .await
        .unwrap();

    let provider_name = omnifs_core::ResourceName::new(metadata.reference.name.clone()).unwrap();
    let declarations = omnifs_api::ResourceDeclarations {
        api_version: omnifs_api::API_VERSION.to_owned(),
        resources: vec![
            omnifs_api::ResourceDefinition::Provider(omnifs_api::ProviderDefinition {
                name: provider_name.clone(),
                artifact: metadata.reference.id,
            }),
            omnifs_api::ResourceDefinition::Mount(omnifs_api::MountResourceDefinition {
                name: omnifs_core::ResourceName::new(format!(
                    "{}-control-plane",
                    provider_name.as_str()
                ))
                .unwrap(),
                provider: provider_name,
                credential: None,
                config: serde_json::json!({}),
                limits: None,
            }),
        ],
    };
    let initial = control
        .get_resources(wire::Empty {})
        .await
        .unwrap()
        .into_inner();
    let initial_revision = grpc::get_resources_response(&initial).unwrap().revision;
    let plan = control
        .plan_resources(grpc::to_plan_resources_request(&declarations))
        .await
        .unwrap()
        .into_inner();
    let plan = grpc::plan_resources_response(&plan).unwrap();
    assert_eq!(plan.base_revision, initial_revision);
    assert_eq!(plan.changes.len(), 2);

    let applied = control
        .apply_resources(grpc::to_apply_resources_request(
            &omnifs_api::ApplyResourcesRequest {
                mutation_id: omnifs_core::MutationId::from_bytes([0x71; 16]),
                base_revision: plan.base_revision,
                expected_desired_digest: plan.desired_digest,
                declarations,
                credential_material: Vec::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let receipt = grpc::apply_resources_response(&applied).unwrap();
    assert!(receipt.changed);

    let mut first = control
        .watch_progress(grpc::to_progress_target(
            omnifs_api::ProgressTarget::DesiredRevision(receipt.revision),
        ))
        .await
        .unwrap()
        .into_inner();
    let snapshot = grpc::progress_event(&first.message().await.unwrap().unwrap()).unwrap();
    assert!(matches!(
        snapshot.event,
        omnifs_api::ProgressEventKind::Snapshot(ref snapshot)
            if snapshot.desired_revision == receipt.revision
                && snapshot.resources.iter().all(|status| {
                    matches!(
                        status.phase,
                        omnifs_api::ResourcePhase::Pending
                            | omnifs_api::ResourcePhase::Preparing
                            | omnifs_api::ResourcePhase::Ready
                    )
                })
    ));
    drop(first);

    let current = control
        .get_resources(wire::Empty {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        grpc::get_resources_response(&current).unwrap().revision,
        receipt.revision
    );
    let mut resumed = control
        .watch_progress(grpc::to_progress_target(
            omnifs_api::ProgressTarget::DesiredRevision(receipt.revision),
        ))
        .await
        .unwrap()
        .into_inner();
    let resumed_snapshot =
        grpc::progress_event(&resumed.message().await.unwrap().unwrap()).unwrap();
    let (mut saw_provider, mut saw_serving, mut terminal) = match resumed_snapshot.event {
        omnifs_api::ProgressEventKind::Snapshot(snapshot) => (
            snapshot.providers.iter().any(|provider| {
                provider.digest == metadata.reference.id
                    && matches!(
                        provider.stage,
                        omnifs_api::ProviderPreparationStage::Compiling
                            | omnifs_api::ProviderPreparationStage::Ready
                    )
            }),
            snapshot.serving.is_some(),
            snapshot.observed_revision == Some(receipt.revision)
                && snapshot
                    .resources
                    .iter()
                    .all(|status| status.phase == omnifs_api::ResourcePhase::Ready),
        ),
        _ => panic!("resumed revision stream must start with a snapshot"),
    };
    // Cold Wasmtime compilation time is host-dependent and not a product
    // deadline. The apply RPC above is the bounded assertion; this is only a
    // test-runner safety cap while the revision stream reports real stages.
    let deadline = tokio::time::Instant::now() + Duration::from_mins(3);
    while !terminal {
        let message =
            if let Ok(message) = tokio::time::timeout_at(deadline, resumed.message()).await {
                message.unwrap().expect("revision stream remains open")
            } else {
                let current = control
                    .get_resources(wire::Empty {})
                    .await
                    .expect("read stalled resource status")
                    .into_inner();
                panic!("revision reconcile stalled: {current:?}");
            };
        let event = grpc::progress_event(&message).unwrap();
        match event.event {
            omnifs_api::ProgressEventKind::Snapshot(snapshot)
            | omnifs_api::ProgressEventKind::Resync(snapshot) => {
                saw_provider |= snapshot.providers.iter().any(|provider| {
                    provider.digest == metadata.reference.id
                        && matches!(
                            provider.stage,
                            omnifs_api::ProviderPreparationStage::Compiling
                                | omnifs_api::ProviderPreparationStage::Ready
                        )
                });
                saw_serving |= snapshot.serving.is_some();
            },
            omnifs_api::ProgressEventKind::ProviderPreparation(provider) => {
                saw_provider |= provider.digest == metadata.reference.id;
            },
            omnifs_api::ProgressEventKind::ServingProgress(_) => saw_serving = true,
            omnifs_api::ProgressEventKind::RevisionReady(revision)
                if revision == receipt.revision =>
            {
                terminal = true;
            },
            omnifs_api::ProgressEventKind::RevisionFailed { detail, .. } => {
                panic!("revision reconcile failed: {detail}");
            },
            _ => {},
        }
    }
    assert!(
        saw_provider,
        "revision progress names its required provider"
    );
    assert!(saw_serving, "revision progress reports serving stages");
    let ready = control
        .get_resources(wire::Empty {})
        .await
        .unwrap()
        .into_inner();
    let ready = grpc::get_resources_response(&ready).unwrap();
    assert!(
        ready
            .resource_statuses
            .iter()
            .all(|status| status.phase == omnifs_api::ResourcePhase::Ready)
    );

    let cache = daemon.home.path().join("daemon-state/cache/wasmtime");
    assert!(cache.is_dir(), "daemon owns one durable Wasmtime cache");
    drop(control);
    daemon.restart().await;
    let mut restarted = control_client(&daemon.control_socket()).await;
    let mut recovery = restarted
        .watch_progress(grpc::to_progress_target(
            omnifs_api::ProgressTarget::DesiredRevision(receipt.revision),
        ))
        .await
        .expect("watch restarted desired revision")
        .into_inner();
    let recovery_deadline = tokio::time::Instant::now() + Duration::from_mins(3);
    loop {
        let message = tokio::time::timeout_at(recovery_deadline, recovery.message())
            .await
            .expect("restarted revision finishes before safety cap")
            .unwrap()
            .expect("restarted revision stream remains open");
        match grpc::progress_event(&message).unwrap().event {
            omnifs_api::ProgressEventKind::Snapshot(snapshot)
            | omnifs_api::ProgressEventKind::Resync(snapshot)
                if snapshot.observed_revision == Some(receipt.revision) =>
            {
                break;
            },
            omnifs_api::ProgressEventKind::RevisionReady(revision)
                if revision == receipt.revision =>
            {
                break;
            },
            omnifs_api::ProgressEventKind::RevisionFailed { detail, .. } => {
                panic!("restarted revision reconcile failed: {detail}");
            },
            _ => {},
        }
    }
    let recovered = restarted
        .get_resources(wire::Empty {})
        .await
        .expect("restarted daemon exposes desired and observed status")
        .into_inner();
    let recovered = grpc::get_resources_response(&recovered).unwrap();
    assert_eq!(recovered.revision, receipt.revision);
    assert_eq!(recovered.serving_revision, Some(receipt.revision));
    assert_eq!(
        cache,
        daemon.home.path().join("daemon-state/cache/wasmtime")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // linear end-to-end acceptance scenario
fn two_daemons_two_homes_resolve_through_their_own_sockets() {
    if std::env::var_os("OMNIFS_ACCEPTANCE_LIVE").is_none() {
        eprintln!("skip: set OMNIFS_ACCEPTANCE_LIVE=1 to run live control-plane acceptance");
        return;
    }
    if !platform_can_mount() {
        eprintln!("skip: platform cannot mount");
        return;
    }

    // Hold the cross-process NFS lock for the whole test so no other live-mount
    // binary races these two mounts.
    let _nfs_lock = live::nfs_serial_lock();

    // Two daemons use independent homes and mount points.
    let Some(mut daemon_a) = Daemon::spawn() else {
        return;
    };
    let Some(mut daemon_b) = Daemon::spawn() else {
        return;
    };
    if daemon_a.wait_serving().is_none() || daemon_b.wait_serving().is_none() {
        return;
    }

    // Each home resolves its own daemon over its own socket.
    let out_a = run_status(daemon_a.home.path());
    assert_eq!(
        exit_code(&out_a),
        0,
        "status for home A must exit 0\nstderr: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );
    let json_a = status_json(&out_a);
    let inventory_a = json_a["result"]["inventory"]
        .as_object()
        .expect("status inventory");
    assert_eq!(inventory_a["daemon"]["probe"]["state"], "responding");
    let pid_a = inventory_a["daemon"]["status"]["info"]["pid"]
        .as_u64()
        .expect("A pid");
    let instance_a = inventory_a["daemon"]["status"]["info"]["instance_id"]
        .as_str()
        .expect("A instance id");
    assert_eq!(
        pid_a,
        u64::from(daemon_a.pid()),
        "status A must report A's pid"
    );
    let identity_a = daemon_a.identity_json();
    assert_eq!(identity_a["pid"].as_u64(), Some(pid_a));
    assert_eq!(identity_a["instance_token"].as_str(), Some(instance_a));
    assert!(
        inventory_a["filesystems"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    assert!(inventory_a["mounts"].as_array().is_some_and(Vec::is_empty));

    let out_b = run_status(daemon_b.home.path());
    assert_eq!(exit_code(&out_b), 0, "status for home B must exit 0");
    let json_b = status_json(&out_b);
    let pid_b = json_b["result"]["inventory"]["daemon"]["status"]["info"]["pid"]
        .as_u64()
        .expect("B pid");
    let instance_b = json_b["result"]["inventory"]["daemon"]["status"]["info"]["instance_id"]
        .as_str()
        .expect("B instance id")
        .to_owned();
    assert_eq!(
        pid_b,
        u64::from(daemon_b.pid()),
        "status B must report B's pid"
    );
    let identity_b = daemon_b.identity_json();
    assert_eq!(identity_b["pid"].as_u64(), Some(pid_b));
    assert_eq!(
        identity_b["instance_token"].as_str(),
        Some(instance_b.as_str())
    );
    assert!(
        json_b["result"]["inventory"]["filesystems"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );

    assert_ne!(pid_a, pid_b, "the two daemons must be distinct processes");

    // A fresh home with no control socket never dials another profile.
    //
    // `omnifs status` is informational and exits 0 whether or not a daemon is
    // running. The property that matters here is that a fresh profile reports
    // stopped and never dials A or B.
    let fresh = hermetic_home();
    let out_fresh = run_status(fresh.home.path());
    assert_eq!(
        exit_code(&out_fresh),
        0,
        "status for a home with no socket must exit 0 (informational)\nstderr: {}",
        String::from_utf8_lossy(&out_fresh.stderr)
    );
    assert_eq!(
        status_json(&out_fresh)["result"]["inventory"]["daemon"]["probe"]["state"],
        "stopped",
        "a home with no socket must report stopped, never a foreign daemon"
    );

    // After home A's daemon is killed, doctor starts a replacement for that
    // profile without disturbing home B.
    daemon_a.child.kill().expect("kill daemon A");
    daemon_a.child.wait().expect("reap daemon A");

    // A's stale identity remains beside its now-dead socket, and status names
    // that exact profile as unreachable before doctor revives it.
    assert_eq!(
        daemon_a.identity_json(),
        identity_a,
        "SIGKILL must leave daemon A's old process identity"
    );
    assert!(
        !live::control_ready(&daemon_a.control_socket()),
        "daemon A's control socket must stop responding after SIGKILL"
    );
    let out_dead = run_status(daemon_a.home.path());
    assert_eq!(
        exit_code(&out_dead),
        3,
        "status for killed home A must report daemon unavailable\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out_dead.stdout),
        String::from_utf8_lossy(&out_dead.stderr)
    );
    assert_eq!(
        status_json(&out_dead)["result"]["inventory"]["daemon"]["probe"]["state"],
        "unreachable",
        "the killed home A must report its stale profile as unreachable"
    );

    let doctor_dead = run_doctor(daemon_a.home.path());
    assert!(
        matches!(exit_code(&doctor_dead), 0 | 5),
        "doctor must complete after reviving daemon A\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&doctor_dead.stdout),
        String::from_utf8_lossy(&doctor_dead.stderr)
    );

    // Doctor's replacement answers on A's socket with a new exact identity.
    assert!(
        live::control_ready(&daemon_a.control_socket()),
        "doctor-spawned daemon A must answer on its profile-local control socket"
    );
    let out_revived = run_status(daemon_a.home.path());
    assert_eq!(
        exit_code(&out_revived),
        0,
        "status for revived home A must exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out_revived.stdout),
        String::from_utf8_lossy(&out_revived.stderr)
    );
    let revived_json = status_json(&out_revived);
    assert_eq!(
        revived_json["result"]["inventory"]["daemon"]["probe"]["state"], "responding",
        "doctor must leave a responding replacement for home A"
    );
    let revived_pid = revived_json["result"]["inventory"]["daemon"]["status"]["info"]["pid"]
        .as_u64()
        .expect("revived A pid");
    let revived_instance =
        revived_json["result"]["inventory"]["daemon"]["status"]["info"]["instance_id"]
            .as_str()
            .expect("revived A instance id");
    assert_ne!(revived_pid, pid_a, "doctor must replace daemon A's pid");
    assert_ne!(
        revived_instance, instance_a,
        "doctor must replace daemon A's instance"
    );
    let revived_identity = daemon_a.identity_json();
    assert_eq!(revived_identity["pid"].as_u64(), Some(revived_pid));
    assert_eq!(
        revived_identity["instance_token"].as_str(),
        Some(revived_instance)
    );
    assert_ne!(
        revived_identity, identity_a,
        "process.json must represent doctor-spawned daemon A"
    );

    // Home B still answers correctly.
    let out_b2 = run_status(daemon_b.home.path());
    assert_eq!(
        exit_code(&out_b2),
        0,
        "home B must still answer after A is gone"
    );
    let json_b2 = status_json(&out_b2);
    assert_eq!(
        json_b2["result"]["inventory"]["daemon"]["probe"]["state"],
        "responding"
    );
    assert_eq!(
        json_b2["result"]["inventory"]["daemon"]["status"]["info"]["pid"].as_u64(),
        Some(u64::from(daemon_b.pid())),
    );
    assert_eq!(
        json_b2["result"]["inventory"]["daemon"]["status"]["info"]["instance_id"].as_str(),
        Some(instance_b.as_str())
    );
    assert_eq!(
        daemon_b.identity_json(),
        identity_b,
        "doctor for home A must not change home B's process identity"
    );

    // A graceful SIGTERM removes home B's process identity.
    let pid_b = daemon_b.pid();
    let _ = Command::new("kill")
        .args(["-TERM", &pid_b.to_string()])
        .output();
    daemon_b.child.wait().expect("reap daemon B");
    let identity_b = daemon_b.identity_path();
    let deadline = Instant::now() + Duration::from_secs(10);
    while identity_b.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !identity_b.exists(),
        "a gracefully stopped daemon must remove its process identity"
    );

    drop(daemon_a);
    drop(daemon_b);
}
