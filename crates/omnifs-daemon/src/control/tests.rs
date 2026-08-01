//! Control-plane integration tests: one live daemon over a real socket.

use super::*;
use crate::daemon::DaemonParts;
use hyper_util::rt::TokioIo;
use omnifs_api::{
    FilesystemDefinition, NormalizedResourceSet, ProviderImportDisposition, ResourceDefinition,
};
use omnifs_core::{FilesystemProtocol, FilesystemRuntime, FilesystemSpec, ResourceName};
use omnifs_state::{FilesystemObservation, FilesystemPhase, ResourceApplyRequest};
use tokio_stream::StreamExt as _;
use tonic::transport::Endpoint;
use tower::service_fn;

type Client = wire::control_client::ControlClient<tonic::transport::Channel>;

async fn client(path: &std::path::Path) -> Client {
    try_client(path).await.unwrap()
}

async fn try_client(path: &std::path::Path) -> Result<Client, tonic::transport::Error> {
    let socket = path.to_owned();
    let channel = Endpoint::try_from("http://[::]:50051")
        .expect("valid test endpoint")
        .connect_with_connector(service_fn(move |_| {
            let socket = socket.clone();
            async move {
                tokio::net::UnixStream::connect(socket)
                    .await
                    .map(TokioIo::new)
            }
        }))
        .await?;
    Ok(Client::new(channel))
}

fn provider_wasm() -> Vec<u8> {
    let metadata = serde_json::to_vec(&serde_json::json!({
        "id": "demo",
        "displayName": "Demo",
        "description": "A test provider",
        "provider": "demo.wasm",
        "defaultMount": "demo",
        "refreshIntervalSecs": 0,
        "capabilities": [{
            "kind": "domain",
            "value": "api.demo.test",
            "why": "Test credential injection."
        }],
        "auth": {
            "default": "pat",
            "schemes": [{
                "staticToken": {
                    "key": "pat",
                    "valuePrefix": "Bearer ",
                    "description": "Demo token",
                    "injectDomains": ["api.demo.test"]
                }
            }]
        }
    }))
    .unwrap();
    let name = omnifs_provider::PROVIDER_METADATA_SECTION_NAME.as_bytes();
    let mut payload = Vec::new();
    append_uleb(&mut payload, name.len());
    payload.extend_from_slice(name);
    payload.extend_from_slice(&metadata);

    let mut wasm = b"\0asm\x01\0\0\0".to_vec();
    wasm.push(0);
    append_uleb(&mut wasm, payload.len());
    wasm.extend_from_slice(&payload);
    wasm
}

fn append_uleb(output: &mut Vec<u8>, mut value: usize) {
    loop {
        let byte = u8::try_from(value & 0x7f).unwrap();
        value >>= 7;
        if value == 0 {
            output.push(byte);
            return;
        }
        output.push(byte | 0x80);
    }
}

/// Decode the structured error code an aborted/failed-precondition status
/// carries in its details, so tests assert on the domain code rather than
/// just the transport-level `tonic::Code`.
fn error_code(status: &tonic::Status) -> omnifs_api::ControlErrorCode {
    grpc::error_detail(&wire::ErrorDetail::decode(status.details()).unwrap())
        .unwrap()
        .code
}

async fn import_provider_bytes(
    c: &mut Client,
    bytes: Vec<u8>,
) -> omnifs_api::ProviderImportReceipt {
    let digest = omnifs_core::ProviderId::from_wasm_bytes(&bytes);
    let start = grpc::to_provider_upload_start("demo.wasm", bytes.len() as u64, &digest);
    let upload = tokio_stream::iter([
        wire::ImportProviderRequest {
            value: Some(wire::import_provider_request::Value::Start(start)),
        },
        wire::ImportProviderRequest {
            value: Some(wire::import_provider_request::Value::Chunk(bytes.into())),
        },
    ]);
    let response = c.import_provider(upload).await.unwrap().into_inner();
    grpc::provider_import_receipt(response.receipt.as_ref().unwrap()).unwrap()
}

async fn import_test_provider(c: &mut Client, bytes: Vec<u8>) -> omnifs_core::ProviderId {
    import_provider_bytes(c, bytes).await.provider.id
}

struct TestRuntime {
    daemon: Arc<super::Daemon>,
    control: Arc<super::ControlServer>,
    shutdown: tokio::sync::watch::Sender<bool>,
    control_task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl TestRuntime {
    async fn shutdown(self) {
        self.control.set_shutting_down();
        let _ = self.shutdown.send(true);
        self.control_task.await.unwrap().unwrap();
        self.daemon.shutdown().await.unwrap();
    }
}

async fn test_daemon(dir: &tempfile::TempDir) -> TestRuntime {
    test_daemon_with_limits(dir, CONTROL_CONNECTION_LIMIT, CONTROL_PREFACE_TIMEOUT).await
}

async fn test_daemon_with_limits(
    dir: &tempfile::TempDir,
    connection_limit: usize,
    preface_timeout: std::time::Duration,
) -> TestRuntime {
    let profile = omnifs_bootstrap::Profile::under_root(dir.path());
    let state_paths = omnifs_state::DaemonStatePaths::new(profile.root().join("daemon-state"));
    let context = Arc::new(crate::context::DaemonContext::new(profile, state_paths).unwrap());
    context.prepare_startup_dirs().unwrap();
    let listener = context.bind_control_socket().unwrap();
    let state = Arc::new(
        omnifs_state::StateStore::open(
            context.state_paths().clone(),
            omnifs_state::StateStoreOptions::default(),
        )
        .await
        .unwrap(),
    );
    let paths = state.engine_paths();
    let host = Arc::new(
        omnifs_engine::HostOnline::open_runtime(omnifs_engine::HostRuntimeOpen {
            projection: paths.projection_cache().to_path_buf(),
            clones: paths.clone_cache().to_path_buf(),
            engine: omnifs_engine::ComponentEngine::new(paths.wasmtime_cache()).unwrap(),
        })
        .unwrap(),
    );
    let draft = crate::generation_builder::GenerationDraft::load_resources(&state)
        .await
        .unwrap();
    let parts = draft.prepare(&state, &host).await.unwrap().into_parts();
    let serving =
        omnifs_engine::ServingCell::new(context.namespace_epoch().daemon_instance(), parts.ready);
    parts.pending_refreshes.activate(&state).await.unwrap();
    state.mark_serving(parts.revision).await.unwrap();
    let resources =
        crate::resource_control::ResourceControl::new(Arc::clone(&state), context.instance_id())
            .await
            .unwrap();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let embedded = Arc::new(super::EmbeddedProviders::default());
    let daemon = Arc::new(super::Daemon::new(DaemonParts {
        context: Arc::clone(&context),
        embedded: Arc::clone(&embedded),
        state,
        serving,
        resources,
        inspector: None,
        shutdown_tx: shutdown.clone(),
    }));
    let (control, _repairs) = super::ControlServer::new_with_limits(
        Arc::clone(&context),
        embedded,
        shutdown.clone(),
        connection_limit,
        preface_timeout,
    );
    control.set_ready(Arc::clone(&daemon));
    let control_task = tokio::spawn(Arc::clone(&control).run(listener));
    TestRuntime {
        daemon,
        control,
        shutdown,
        control_task,
    }
}

fn doctor_filesystem_spec(name: &str) -> FilesystemSpec {
    let protocol = if cfg!(target_os = "linux") {
        FilesystemProtocol::Fuse
    } else {
        FilesystemProtocol::Nfs
    };
    FilesystemSpec::new(
        protocol,
        FilesystemRuntime::Host,
        format!("/tmp/omnifs-control-doctor-{name}").into(),
        None,
        None,
    )
    .unwrap()
}

fn doctor_filesystem_set(names: &[&str]) -> NormalizedResourceSet {
    NormalizedResourceSet::new(
        names
            .iter()
            .map(|&name| {
                ResourceDefinition::Filesystem(FilesystemDefinition {
                    name: ResourceName::new(name).unwrap(),
                    spec: doctor_filesystem_spec(name),
                })
            })
            .collect(),
    )
    .unwrap()
}

async fn apply_doctor_filesystems(runtime: &TestRuntime, names: &[&str], mutation: u8) {
    let desired = doctor_filesystem_set(names);
    let snapshot = runtime.daemon.state.resource_snapshot().await.unwrap();
    runtime
        .daemon
        .state
        .apply_resources(ResourceApplyRequest {
            mutation_id: omnifs_core::MutationId::from_bytes([mutation; 16]),
            base_revision: snapshot.revision,
            expected_desired_digest: desired.digest(),
            desired,
            credential_secrets: Vec::new(),
        })
        .await
        .unwrap();
}

async fn mark_doctor_filesystem_starting(runtime: &TestRuntime, name: &str) {
    let name = ResourceName::new(name).unwrap();
    let instance = runtime
        .daemon
        .state
        .filesystem_instance(&name)
        .await
        .unwrap()
        .unwrap();
    let mut observation = FilesystemObservation::from_instance(&instance);
    observation.phase = FilesystemPhase::Starting;
    runtime
        .daemon
        .state
        .write_filesystem_observation(observation)
        .await
        .unwrap()
        .unwrap();
}

fn seed_doctor_host_record(runtime: &TestRuntime, name: &str) -> omnifs_mtab::RunnerRecord {
    let filesystem = ResourceName::new(name).unwrap();
    let state_dir = runtime
        .daemon
        .context
        .state_paths()
        .filesystem_runtime(&filesystem);
    std::fs::create_dir_all(&state_dir).unwrap();
    let record = omnifs_mtab::RunnerRecord {
        version: omnifs_mtab::RunnerRecord::VERSION,
        instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
        pid: 42,
        process_group: 42,
        filesystem,
        spec: doctor_filesystem_spec(name),
        control_socket: state_dir.join("missing-control.sock"),
    };
    std::fs::write(
        state_dir.join("runner.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    record
}

fn seeded_doctor_host_record_path(runtime: &TestRuntime, name: &str) -> std::path::PathBuf {
    runtime
        .daemon
        .context
        .state_paths()
        .filesystem_runtime(&ResourceName::new(name).unwrap())
        .join("runner.json")
}

#[tokio::test]
#[allow(unsafe_code)]
async fn tonic_reports_starting_state_and_invalid_requests() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let profile = omnifs_bootstrap::Profile::resolve().unwrap();
    let state_paths = omnifs_state::DaemonStatePaths::new(profile.root().join("daemon-state"));
    let context = Arc::new(crate::context::DaemonContext::new(profile, state_paths).unwrap());
    context.prepare_startup_dirs().unwrap();
    let listener = context.bind_control_socket().unwrap();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let (control, _repairs) = super::ControlServer::new(
        Arc::clone(&context),
        Arc::new(super::EmbeddedProviders::default()),
        shutdown.clone(),
    );
    let task = tokio::spawn(Arc::clone(&control).run(listener));
    let mut c = client(&context.control_socket()).await;
    let recovery = c
        .get_recovery_state(wire::Empty {})
        .await
        .unwrap()
        .into_inner()
        .recovery
        .unwrap();
    assert_eq!(
        grpc::daemon_recovery(&recovery).unwrap().phase,
        DaemonPhase::Starting
    );
    let status = c.ready(wire::Empty {}).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    let resources = c.get_resources(wire::Empty {}).await.unwrap_err();
    assert_eq!(error_code(&resources), ControlErrorCode::NotReady);
    let doctor = c.run_doctor(wire::Empty {}).await.unwrap_err();
    assert_eq!(error_code(&doctor), ControlErrorCode::NotReady);
    let repairs = c
        .apply_doctor_repairs(wire::ApplyDoctorRepairsRequest { ids: Vec::new() })
        .await
        .unwrap_err();
    assert_eq!(error_code(&repairs), ControlErrorCode::NotReady);
    control.set_recovery(DaemonRecovery {
        phase: DaemonPhase::RecoveryRequired,
        durable_revision: None,
        serving_revision: None,
        store_health: HealthReport::new(HealthState::Unhealthy, "test recovery"),
        repair: None,
    });
    let resources = c.get_resources(wire::Empty {}).await.unwrap_err();
    assert_eq!(error_code(&resources), ControlErrorCode::RecoveryRequired);
    let doctor = c.run_doctor(wire::Empty {}).await.unwrap_err();
    assert_eq!(error_code(&doctor), ControlErrorCode::RecoveryRequired);
    let repairs = c
        .apply_doctor_repairs(wire::ApplyDoctorRepairsRequest { ids: Vec::new() })
        .await
        .unwrap_err();
    assert_eq!(error_code(&repairs), ControlErrorCode::RecoveryRequired);
    let invalid = c
        .repair_state(wire::RepairStateRequest::default())
        .await
        .unwrap_err();
    assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
    let _ = shutdown.send(true);
    task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn tonic_doctor_round_trips_on_ready_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let response = c.run_doctor(wire::Empty {}).await.unwrap().into_inner();
    let report = grpc::run_doctor_response(&response).unwrap();
    assert!(!report.findings.is_empty());
    assert!(report.remediations.is_empty());

    let response = c
        .apply_doctor_repairs(wire::ApplyDoctorRepairsRequest { ids: Vec::new() })
        .await
        .unwrap()
        .into_inner();
    assert!(
        grpc::apply_doctor_repairs_response(&response)
            .unwrap()
            .is_empty()
    );
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn tonic_doctor_does_not_offer_repairs_for_owned_filesystem_states() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let names = ["owned-desired", "owned-starting", "owned-deleting"];
    for name in names {
        seed_doctor_host_record(&runtime, name);
    }
    apply_doctor_filesystems(&runtime, &names, 0x71).await;
    mark_doctor_filesystem_starting(&runtime, "owned-starting").await;
    apply_doctor_filesystems(&runtime, &["owned-desired", "owned-starting"], 0x72).await;

    assert_eq!(
        runtime
            .daemon
            .state
            .filesystem_instance(&ResourceName::new("owned-desired").unwrap())
            .await
            .unwrap()
            .unwrap()
            .phase,
        FilesystemPhase::Pending
    );
    assert_eq!(
        runtime
            .daemon
            .state
            .filesystem_instance(&ResourceName::new("owned-starting").unwrap())
            .await
            .unwrap()
            .unwrap()
            .phase,
        FilesystemPhase::Starting
    );
    assert_eq!(
        runtime
            .daemon
            .state
            .filesystem_instance(&ResourceName::new("owned-deleting").unwrap())
            .await
            .unwrap()
            .unwrap()
            .phase,
        FilesystemPhase::Deleting
    );

    let mut client = client(&home.join("control.sock")).await;
    let response = client
        .run_doctor(wire::Empty {})
        .await
        .unwrap()
        .into_inner();
    let report = grpc::run_doctor_response(&response).unwrap();
    assert!(report.remediations.is_empty());
    for name in names {
        let finding = report
            .findings
            .iter()
            .find(|finding| {
                finding
                    .target
                    .as_deref()
                    .is_some_and(|target| target.contains(name))
            })
            .unwrap_or_else(|| panic!("missing doctor finding for {name}"));
        assert_eq!(finding.fix, None);
        assert_eq!(finding.remediation_id, None);
    }
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn tonic_apply_doctor_repairs_rechecks_desired_and_observed_state() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let desired_record = seed_doctor_host_record(&runtime, "race-desired");
    let observed_record = seed_doctor_host_record(&runtime, "race-observed");

    let mut client = client(&home.join("control.sock")).await;
    let response = client
        .run_doctor(wire::Empty {})
        .await
        .unwrap()
        .into_inner();
    let report = grpc::run_doctor_response(&response).unwrap();
    let desired_offer = report
        .remediations
        .iter()
        .find(|offer| offer.command_line.contains("race-desired"))
        .cloned()
        .expect("doctor did not offer cleanup for race-desired");
    let observed_offer = report
        .remediations
        .iter()
        .find(|offer| offer.command_line.contains("race-observed"))
        .cloned()
        .expect("doctor did not offer cleanup for race-observed");
    assert_eq!(report.remediations.len(), 2);

    apply_doctor_filesystems(&runtime, &["race-desired"], 0x81).await;
    apply_doctor_filesystems(&runtime, &["race-desired", "race-observed"], 0x82).await;
    apply_doctor_filesystems(&runtime, &["race-desired"], 0x83).await;
    assert_eq!(
        runtime
            .daemon
            .state
            .filesystem_instance(&ResourceName::new("race-observed").unwrap())
            .await
            .unwrap()
            .unwrap()
            .phase,
        FilesystemPhase::Deleting
    );

    let response = client
        .apply_doctor_repairs(wire::ApplyDoctorRepairsRequest {
            ids: vec![desired_offer.id.clone(), observed_offer.id.clone()],
        })
        .await
        .unwrap()
        .into_inner();
    let outcomes = grpc::apply_doctor_repairs_response(&response).unwrap();
    assert_eq!(outcomes.len(), 2);
    let desired_outcome = outcomes
        .iter()
        .find(|outcome| outcome.id == desired_offer.id)
        .unwrap();
    assert_eq!(
        desired_outcome.state,
        omnifs_api::DoctorRepairState::Skipped
    );
    assert_eq!(
        desired_outcome.error.as_deref(),
        Some("filesystem became desired since diagnosis")
    );
    let observed_outcome = outcomes
        .iter()
        .find(|outcome| outcome.id == observed_offer.id)
        .unwrap();
    assert_eq!(
        observed_outcome.state,
        omnifs_api::DoctorRepairState::Skipped
    );
    assert_eq!(
        observed_outcome.error.as_deref(),
        Some("filesystem became observed since diagnosis")
    );

    assert_eq!(
        omnifs_mtab::RunnerRecord::read(
            seeded_doctor_host_record_path(&runtime, "race-desired")
                .parent()
                .unwrap()
        )
        .unwrap(),
        Some(desired_record)
    );
    assert_eq!(
        omnifs_mtab::RunnerRecord::read(
            seeded_doctor_host_record_path(&runtime, "race-observed")
                .parent()
                .unwrap()
        )
        .unwrap(),
        Some(observed_record)
    );
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn idle_control_peer_releases_connection_capacity_after_preface_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon_with_limits(&dir, 1, std::time::Duration::from_secs(2)).await;
    let idle = tokio::net::UnixStream::connect(home.join("control.sock"))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while runtime.control.connection_permits.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("idle peer did not consume connection capacity");

    let mut rejected = tokio::net::UnixStream::connect(home.join("control.sock"))
        .await
        .unwrap();
    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout(std::time::Duration::from_secs(1), rejected.read(&mut byte))
        .await
        .expect("connection above the capacity limit stayed open")
        .expect("read rejected connection");
    assert_eq!(
        read, 0,
        "connection above the capacity limit was not closed"
    );

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while runtime.control.connection_permits.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("idle peer did not release connection capacity");
    let mut valid = client(&home.join("control.sock")).await;
    valid.get_status(wire::Empty {}).await.unwrap();
    drop(idle);
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn established_tonic_channel_remains_usable_after_preface_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon_with_limits(&dir, 1, std::time::Duration::from_millis(20)).await;
    let mut c = client(&home.join("control.sock")).await;
    c.get_status(wire::Empty {}).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    c.get_status(wire::Empty {}).await.unwrap();
    runtime.shutdown().await;
}

#[tokio::test]
#[allow(unsafe_code)]
async fn tonic_log_stream_preserves_raw_tail() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let log = runtime.daemon.state.daemon_log_path();
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, b"one\xff\ntwo\nthree\n").unwrap();
    let mut c = client(&home.join("control.sock")).await;
    let mut stream = c
        .stream_logs(wire::StreamLogsRequest {
            follow: false,
            tail_lines: 3,
        })
        .await
        .unwrap()
        .into_inner();
    let ready = stream.next().await.unwrap().unwrap();
    assert!(matches!(
        ready.value,
        Some(wire::log_stream_item::Value::Ready(_))
    ));
    let data = stream.next().await.unwrap().unwrap();
    match data.value {
        Some(wire::log_stream_item::Value::Data(bytes)) => {
            assert_eq!(&bytes[..], b"one\xff\ntwo\nthree\n");
        },
        _ => panic!("missing log data"),
    }
    assert!(stream.next().await.is_none());
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn tonic_provider_upload_reports_inserted_then_unchanged_disposition() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let bytes = provider_wasm();
    let first = import_provider_bytes(&mut c, bytes.clone()).await;
    assert_eq!(first.disposition, ProviderImportDisposition::Inserted);
    let duplicate = import_provider_bytes(&mut c, bytes).await;
    assert_eq!(duplicate.disposition, ProviderImportDisposition::Unchanged);
    assert_eq!(duplicate.provider, first.provider);
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn tonic_followed_log_stream_ends_cleanly_when_daemon_stops() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let log = runtime.daemon.state.daemon_log_path();
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, b"tail\n").unwrap();
    let mut c = client(&home.join("control.sock")).await;
    let mut stream = c
        .stream_logs(wire::StreamLogsRequest {
            follow: true,
            tail_lines: 1,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(matches!(
        stream.next().await.unwrap().unwrap().value,
        Some(wire::log_stream_item::Value::Ready(_))
    ));
    assert!(matches!(
        stream.next().await.unwrap().unwrap().value,
        Some(wire::log_stream_item::Value::Data(data)) if &data[..] == b"tail\n"
    ));
    runtime.shutdown().await;
    assert!(stream.next().await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
async fn tonic_inventory_and_status_use_generated_messages() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let _ = runtime.daemon.start().await.unwrap();
    c.ready(wire::Empty {}).await.unwrap();
    let inventory = c
        .get_inventory(wire::Empty {})
        .await
        .unwrap()
        .into_inner()
        .inventory
        .unwrap();
    assert_eq!(
        grpc::daemon_inventory(&inventory).unwrap().phase,
        DaemonPhase::Ready
    );
    let inventory = grpc::daemon_inventory(&inventory).unwrap();
    assert_eq!(inventory.durable_revision, inventory.serving_revision);
    assert!(inventory.mounts.is_empty());
    assert!(inventory.credentials.is_empty());
    assert!(inventory.filesystems.is_empty());
    let status = c
        .get_status(wire::Empty {})
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    let status = grpc::daemon_status(&status).unwrap();
    assert!(status.mounts.is_empty());
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[allow(unsafe_code)]
#[allow(clippy::too_many_lines)]
async fn typed_resources_apply_fast_and_progress_recovers_after_disconnect() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = crate::ENV_LOCK.lock().await;
    let home = std::fs::canonicalize(dir.path()).unwrap();
    unsafe {
        std::env::set_var("OMNIFS_HOME", &home);
    }
    let runtime = test_daemon(&dir).await;
    let mut c = client(&home.join("control.sock")).await;
    let provider = import_test_provider(&mut c, provider_wasm()).await;
    let provider_name = omnifs_core::ResourceName::new("demo").unwrap();
    let credential_name = omnifs_core::ResourceName::new("alice").unwrap();
    let declarations = omnifs_api::ResourceDeclarations {
        api_version: omnifs_api::API_VERSION.to_owned(),
        resources: vec![
            omnifs_api::ResourceDefinition::Provider(omnifs_api::ProviderDefinition {
                name: provider_name.clone(),
                artifact: provider,
            }),
            omnifs_api::ResourceDefinition::Credential(omnifs_api::CredentialDefinition {
                name: credential_name.clone(),
                provider: provider_name.clone(),
                scheme: "pat".to_owned(),
                account: "alice".to_owned(),
            }),
            omnifs_api::ResourceDefinition::Mount(omnifs_api::MountResourceDefinition {
                name: omnifs_core::ResourceName::new("demo-mount").unwrap(),
                provider: provider_name,
                credential: Some(credential_name.clone()),
                config: serde_json::json!({}),
                limits: None,
            }),
        ],
    };
    let plan_wire = c
        .plan_resources(grpc::to_plan_resources_request(&declarations))
        .await
        .unwrap()
        .into_inner();
    let plan = grpc::plan_resources_response(&plan_wire).unwrap();
    assert_eq!(plan.base_revision, omnifs_core::ResourceRevision::new(0));
    assert_eq!(plan.changes.len(), 3);

    let secret = b"service-boundary-secret";
    let mutation_id = omnifs_core::MutationId::from_bytes([0x51; 16]);
    let request = omnifs_api::ApplyResourcesRequest {
        mutation_id,
        base_revision: plan.base_revision,
        expected_desired_digest: plan.desired_digest,
        declarations: declarations.clone(),
        credential_material: vec![omnifs_api::CredentialMaterialSidecar {
            credential: credential_name.clone(),
            material: omnifs_api::CredentialMaterial::StaticToken {
                token: omnifs_api::SecretBytes::new(secret.to_vec()),
            },
            overrides: omnifs_api::CredentialClientOverrides {
                client_id: None,
                client_secret: None,
                redirect_uri: None,
                scopes: None,
            },
        }],
    };
    let response = c
        .apply_resources(grpc::to_apply_resources_request(&request))
        .await
        .unwrap()
        .into_inner();
    assert!(
        !response
            .encode_to_vec()
            .windows(secret.len())
            .any(|part| part == secret),
        "apply response must not contain credential material"
    );
    let receipt = grpc::apply_resources_response(&response).unwrap();
    assert!(receipt.changed);
    assert_eq!(receipt.revision, plan.base_revision.next().unwrap());

    let retry = c
        .apply_resources(grpc::to_apply_resources_request(&request))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(grpc::apply_resources_response(&retry).unwrap(), receipt);

    let action_id = omnifs_core::ActionId::from_bytes([0x61; 16]);
    let set_request = |token: &[u8]| omnifs_api::SetCredentialMaterialRequest {
        action_id,
        base_action_generation: 0,
        credential: credential_name.clone(),
        material: omnifs_api::CredentialMaterial::StaticToken {
            token: omnifs_api::SecretBytes::new(token.to_vec()),
        },
        overrides: omnifs_api::CredentialClientOverrides {
            client_id: None,
            client_secret: None,
            redirect_uri: None,
            scopes: None,
        },
    };
    let action_response = c
        .set_credential_material(grpc::to_set_credential_material_request(&set_request(
            b"first-action-secret",
        )))
        .await
        .unwrap()
        .into_inner();
    assert!(
        !action_response
            .encode_to_vec()
            .windows(b"first-action-secret".len())
            .any(|part| part == b"first-action-secret")
    );
    let action = grpc::set_credential_material_response(&action_response)
        .unwrap()
        .action;
    assert_eq!(action.action_id, action_id);
    let action_retry = c
        .set_credential_material(grpc::to_set_credential_material_request(&set_request(
            b"different-secret-is-ignored",
        )))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        grpc::set_credential_material_response(&action_retry)
            .unwrap()
            .action,
        action
    );

    let mut first_watch = c
        .watch_progress(grpc::to_progress_target(
            omnifs_api::ProgressTarget::DesiredRevision(receipt.revision),
        ))
        .await
        .unwrap()
        .into_inner();
    let initial = first_watch.message().await.unwrap().unwrap();
    let initial = grpc::progress_event(&initial).unwrap();
    assert!(matches!(
        initial.event,
        omnifs_api::ProgressEventKind::Snapshot(ref snapshot)
            if snapshot.desired_revision == receipt.revision
                && snapshot.resources.iter().all(|status| {
                    status.desired_revision == receipt.revision
                        && status.phase == omnifs_api::ResourcePhase::Pending
                })
    ));
    drop(first_watch);

    let resources = c.get_resources(wire::Empty {}).await.unwrap().into_inner();
    assert_eq!(
        grpc::get_resources_response(&resources).unwrap().revision,
        receipt.revision
    );
    let mut resumed = c
        .watch_progress(grpc::to_progress_target(
            omnifs_api::ProgressTarget::DesiredRevision(receipt.revision),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(matches!(
        grpc::progress_event(&resumed.message().await.unwrap().unwrap())
            .unwrap()
            .event,
        omnifs_api::ProgressEventKind::Snapshot(_)
    ));
    drop(resumed);

    let empty = omnifs_api::ResourceDeclarations {
        api_version: omnifs_api::API_VERSION.to_owned(),
        resources: Vec::new(),
    };
    let empty_digest = empty.clone().normalize().unwrap().digest();
    let stale = c
        .apply_resources(grpc::to_apply_resources_request(
            &omnifs_api::ApplyResourcesRequest {
                mutation_id: omnifs_core::MutationId::from_bytes([0x52; 16]),
                base_revision: omnifs_core::ResourceRevision::new(0),
                expected_desired_digest: empty_digest,
                declarations: empty,
                credential_material: Vec::new(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(error_code(&stale), ControlErrorCode::StaleBaseRevision);

    let unsupported = omnifs_api::ResourceDeclarations {
        api_version: "omnifs.dev/v999".to_owned(),
        resources: Vec::new(),
    };
    let unsupported = c
        .plan_resources(grpc::to_plan_resources_request(&unsupported))
        .await
        .unwrap_err();
    assert_eq!(
        error_code(&unsupported),
        ControlErrorCode::UnsupportedApiVersion
    );

    let unknown_action = c
        .watch_progress(grpc::to_progress_target(
            omnifs_api::ProgressTarget::Action(omnifs_core::ActionId::from_bytes([0x99; 16])),
        ))
        .await
        .unwrap_err();
    assert_eq!(
        error_code(&unknown_action),
        ControlErrorCode::ActionUnavailable
    );
    runtime.shutdown().await;
}
