//! Durable CLI and daemon lifecycle acceptance tests.
//!
//! These tests use a fresh profile for every scenario and talk to the real
//! `omnifs` child process. The state under test belongs to the daemon or the
//! CLI client, so the assertions use the typed local control protocol and the
//! public command surface rather than reaching into implementation helpers.

#![cfg(not(target_os = "wasi"))]

mod common;

use bytes::Bytes;
use common::{omnifs_bin, release_wasm_dir};
use hyper_util::rt::TokioIo;
use omnifs_api::grpc::{self, wire};
use omnifs_api::{
    CONTROL_REQUEST_TIMEOUT_SECS, CONTROL_STREAM_PAYLOAD_MAX_BYTES, DaemonInventory,
    MountDefinition,
};
use omnifs_bootstrap::{Bootstrap, Client};
use omnifs_core::{MountName, ProviderId};
use prost::Message as _;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_stream::iter;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Status};
use tower::service_fn;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
type ControlClient = wire::control_client::ControlClient<Channel>;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(CONTROL_REQUEST_TIMEOUT_SECS);
const PROVIDER_IMPORT_TIMEOUT: Duration = Duration::from_mins(3);
const MUTATION_TIMEOUT: Duration = Duration::from_secs(30);
const PROVIDER_CHUNK_BYTES: usize = CONTROL_STREAM_PAYLOAD_MAX_BYTES;

fn unary<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.set_timeout(REQUEST_TIMEOUT);
    request
}

fn mutation<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.set_timeout(MUTATION_TIMEOUT);
    request
}

fn transient(status: &Status) -> bool {
    matches!(
        status.code(),
        Code::Unavailable | Code::DeadlineExceeded | Code::ResourceExhausted
    )
}

struct Fixture {
    home: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home tempdir"),
        }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    fn endpoint(&self) -> Bootstrap<Client> {
        Bootstrap::<Client>::under_root(self.home_path())
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(omnifs_bin())
            .args(args)
            .env("OMNIFS_HOME", self.home_path())
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "warn")
            .output()
            .unwrap_or_else(|error| panic!("spawn omnifs {}: {error}", args.join(" ")))
    }

    /// A daemon child of the profile, whatever its phase.
    fn spawn_daemon(&self) -> DaemonGuard {
        let child = Command::new(omnifs_bin())
            .arg("daemon")
            .env("OMNIFS_HOME", self.home_path())
            .env("RUST_LOG", "warn")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn omnifs daemon");
        DaemonGuard {
            child: Some(child),
            endpoint: self.endpoint(),
        }
    }

    async fn start_daemon(&self) -> DaemonGuard {
        let mut guard = self.spawn_daemon();
        wait_until_ready(&guard.endpoint).await;
        // A child that exits during the readiness loop must fail the test with
        // its status instead of leaking a process into the next scenario.
        assert!(
            guard
                .child
                .as_mut()
                .expect("daemon child")
                .try_wait()
                .expect("poll daemon")
                .is_none(),
            "daemon exited after reporting ready"
        );
        guard
    }
}

struct DaemonGuard {
    child: Option<Child>,
    endpoint: Bootstrap<Client>,
}

impl DaemonGuard {
    fn reap_if_exited(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if child.try_wait().expect("poll daemon exit").is_some() {
            self.child = None;
        }
    }

    async fn stop(&mut self) {
        let socket = self.endpoint.control_socket();
        if let Ok(mut control) = client(&socket).await {
            let _ = control
                .shutdown(unary(wire::ShutdownRequest {
                    stop_filesystems: false,
                }))
                .await;
        }
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        loop {
            let Some(child) = self.child.as_mut() else {
                return;
            };
            if child.try_wait().expect("poll daemon exit").is_some() {
                self.child = None;
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                self.child = None;
                panic!("daemon did not stop within {}s", STARTUP_TIMEOUT.as_secs());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn wait_until_ready(endpoint: &Bootstrap<Client>) -> DaemonInventory {
    let socket = endpoint.control_socket();
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(mut control) = client(&socket).await {
            match control.get_inventory(unary(wire::Empty {})).await {
                Ok(response) => {
                    if let Some(inventory) = response.into_inner().inventory {
                        let inventory = grpc::daemon_inventory(&inventory)
                            .expect("daemon returned invalid inventory");
                        if inventory.phase == omnifs_api::DaemonPhase::Ready {
                            return inventory;
                        }
                    }
                },
                Err(status) if transient(&status) => {},
                Err(status) => panic!("daemon inventory request failed during startup: {status}"),
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon did not become ready within {}s",
            STARTUP_TIMEOUT.as_secs()
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn client(path: &Path) -> anyhow::Result<ControlClient> {
    let path = path.to_owned();
    let endpoint = Endpoint::from_static("http://[::]:50051").connect_timeout(REQUEST_TIMEOUT);
    let future = endpoint.connect_with_connector(service_fn(move |_| {
        let path = path.clone();
        async move { UnixStream::connect(path).await.map(TokioIo::new) }
    }));
    let channel = tokio::time::timeout(REQUEST_TIMEOUT, future)
        .await
        .map_err(|_| anyhow::anyhow!("control HTTP/2 setup timed out"))??;
    Ok(ControlClient::new(channel))
}

/// Provider import carries no mutation identity: the daemon dedupes by
/// content digest, so this is a plain streamed upload.
async fn import_provider(
    path: &Path,
    bytes: &[u8],
) -> anyhow::Result<omnifs_api::ProviderImportReceipt> {
    let mut control = client(path).await?;
    let start = wire::ImportProviderRequest {
        value: Some(wire::import_provider_request::Value::Start(
            grpc::to_provider_upload_start(
                "test_provider.wasm",
                bytes.len() as u64,
                &ProviderId::from_wasm_bytes(bytes),
            ),
        )),
    };
    let payload = Bytes::copy_from_slice(bytes);
    let mut items = Vec::with_capacity(payload.len().div_ceil(PROVIDER_CHUNK_BYTES) + 1);
    items.push(start);
    for start in (0..payload.len()).step_by(PROVIDER_CHUNK_BYTES) {
        let end = (start + PROVIDER_CHUNK_BYTES).min(payload.len());
        items.push(wire::ImportProviderRequest {
            value: Some(wire::import_provider_request::Value::Chunk(
                payload.slice(start..end),
            )),
        });
    }
    let mut request = Request::new(iter(items));
    request.set_timeout(PROVIDER_IMPORT_TIMEOUT);
    let response = control.import_provider(request).await?.into_inner();
    let receipt = response
        .receipt
        .ok_or_else(|| anyhow::anyhow!("missing provider import receipt"))?;
    grpc::provider_import_receipt(&receipt).map_err(Into::into)
}

fn random_mutation_id() -> Bytes {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("generate mutation id");
    bytes.to_vec().into()
}

fn mutation_id_of(bytes: &Bytes) -> omnifs_core::MutationId {
    omnifs_core::MutationId::from_bytes(bytes.as_ref().try_into().expect("mutation id is 16 bytes"))
}

async fn begin_mutation(control: &mut ControlClient, mutation_id: Bytes) {
    control
        .begin_mutation(mutation(wire::BeginMutationRequest { mutation_id }))
        .await
        .expect("begin mutation");
}

async fn apply_mount_create(
    control: &mut ControlClient,
    mutation_id: Bytes,
    definition: &MountDefinition,
) -> Result<omnifs_api::MountOpResult, Status> {
    let response = control
        .apply_mutation(mutation(wire::ApplyMutationRequest {
            mutation_id,
            ops: vec![wire::MutationOp {
                op: Some(wire::mutation_op::Op::CreateMount(wire::CreateMountOp {
                    definition: Some(grpc::to_mount_definition(definition)),
                })),
            }],
        }))
        .await?
        .into_inner();
    let result = response
        .results
        .into_iter()
        .next()
        .expect("apply reply carries one result per submitted op");
    match result.result.expect("mutation op result missing its op") {
        wire::mutation_op_result::Result::Mount(mount) => {
            Ok(grpc::mount_op_result(&mount).expect("decode mount op result"))
        },
        wire::mutation_op_result::Result::Credential(_) => {
            panic!("expected a mount op result from a mount-create batch")
        },
    }
}

/// The structured `ControlErrorCode` a failed control RPC carries, decoded
/// from its status details the same way the CLI's `rpc.rs` does.
fn control_error_code(status: &Status) -> Option<omnifs_api::ControlErrorCode> {
    let detail = wire::ErrorDetail::decode(status.details()).ok()?;
    grpc::error_detail(&detail).ok().map(|error| error.code)
}

fn mount_definition(provider: ProviderId, name: &str) -> MountDefinition {
    MountDefinition {
        name: MountName::new(name.to_owned()).expect("valid test mount name"),
        provider,
        auth: None,
        limits: None,
        config: br"{}".to_vec(),
    }
}

fn assert_success(output: &Output, command: &str) {
    assert!(
        output.status.success(),
        "{command} failed with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The result of `omnifs --output json status --ready`.
fn ready_status(fixture: &Fixture) -> serde_json::Value {
    let status = fixture.run(&["--output", "json", "status", "--ready"]);
    let json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap_or_else(|error| {
        panic!(
            "status --ready must produce JSON: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        )
    });
    json["result"].clone()
}

/// Waits for `daemon` to bind its control socket, whatever its phase.
async fn wait_for_control_socket(daemon: &DaemonGuard) {
    let socket = daemon.endpoint.control_socket();
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    while !socket.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon did not bind its control socket within {}s",
            STARTUP_TIMEOUT.as_secs()
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// `status --ready` of a daemon that has just bound its control socket gives
/// its status once its `WatchPhase` stream gives it ready.
#[tokio::test(flavor = "multi_thread")]
async fn status_ready_gives_a_starting_daemon_once_it_is_ready() {
    let fixture = Fixture::new();
    let mut daemon = fixture.spawn_daemon();
    wait_for_control_socket(&daemon).await;
    let status = ready_status(&fixture);
    assert_eq!(status["daemon"]["status"]["phase"], "ready");
    daemon.stop().await;
}

/// `status --ready` of a ready daemon gives its status at once.
#[tokio::test(flavor = "multi_thread")]
async fn status_ready_gives_a_ready_daemon_at_once() {
    let fixture = Fixture::new();
    let mut daemon = fixture.start_daemon().await;
    let status = ready_status(&fixture);
    assert_eq!(status["daemon"]["status"]["phase"], "ready");
    daemon.stop().await;
}

/// `status --ready` of a daemon whose control store cannot be opened gives
/// its status with the phase `recovery_required` and the error of its
/// recovery in the health of its state.
#[tokio::test(flavor = "multi_thread")]
async fn status_ready_gives_a_recovering_daemon_with_its_error() {
    let fixture = Fixture::new();
    let store = fixture.home_path().join("daemon-state/control-store");
    std::fs::create_dir_all(&store).expect("control store folder");
    std::fs::write(store.join("state.sqlite3"), b"not sqlite").expect("corrupt control store");
    let mut daemon = fixture.spawn_daemon();
    wait_for_control_socket(&daemon).await;
    let status = ready_status(&fixture);
    assert_eq!(status["daemon"]["status"]["phase"], "recovery_required");
    let error = status["daemon"]["status"]["health"]["mounts"]["message"]
        .as_str()
        .expect("the health of the state of the daemon");
    assert!(
        error.starts_with("control store could not be opened: "),
        "{error}"
    );
    daemon.stop().await;
}

/// A command with `--wait-for-daemon` started without the folder of its
/// profile starts no daemon, and ends with the daemon that another process
/// starts after it, once that daemon is ready.
#[tokio::test(flavor = "multi_thread")]
async fn wait_for_daemon_ends_with_the_daemon_of_another_process_once_it_is_ready() {
    let parent = tempfile::tempdir().expect("parent of the profile");
    let profile = parent.path().join("profile");
    let mut command = Command::new(omnifs_bin())
        .args([
            "--wait-for-daemon",
            "--output",
            "json",
            "mount",
            "rm",
            "absent",
            "--dry-run",
        ])
        .env("OMNIFS_HOME", &profile)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn omnifs with --wait-for-daemon");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        command.try_wait().expect("poll the command").is_none(),
        "the command waits for a daemon"
    );
    assert!(!profile.exists(), "the command starts no daemon");

    let endpoint = Bootstrap::<Client>::under_root(&profile);
    let mut daemon = DaemonGuard {
        child: Some(
            Command::new(omnifs_bin())
                .arg("daemon")
                .env("OMNIFS_HOME", &profile)
                .env("RUST_LOG", "warn")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn omnifs daemon"),
        ),
        endpoint: endpoint.clone(),
    };
    let output = tokio::time::timeout(
        STARTUP_TIMEOUT,
        tokio::task::spawn_blocking(move || command.wait_with_output()),
    )
    .await
    .expect("the command ends with the daemon")
    .expect("join the command")
    .expect("wait for the command");
    assert_success(&output, "mount rm --dry-run with --wait-for-daemon");
    let inventory = client(&endpoint.control_socket())
        .await
        .expect("connect to the daemon")
        .get_inventory(unary(wire::Empty {}))
        .await
        .expect("the inventory of the daemon")
        .into_inner()
        .inventory
        .expect("an inventory");
    assert_eq!(
        grpc::daemon_inventory(&inventory)
            .expect("a valid inventory")
            .phase,
        omnifs_api::DaemonPhase::Ready,
        "the command ends once the daemon is ready"
    );
    daemon.stop().await;
}

/// `status --ready` without a daemon gives the status at once and starts
/// none.
#[tokio::test(flavor = "multi_thread")]
async fn status_ready_without_a_daemon_gives_the_status_at_once() {
    let fixture = Fixture::new();
    let status = ready_status(&fixture);
    assert!(status["daemon"]["status"].is_null(), "{status}");
    assert_eq!(status["daemon"]["probe"]["state"], "stopped");
    assert!(!fixture.endpoint().control_socket().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_starts_and_reports_ready_inventory() {
    let fixture = Fixture::new();
    let mut daemon = fixture.start_daemon().await;
    let mut control = client(&fixture.endpoint().control_socket())
        .await
        .expect("control client");
    let inventory = control
        .get_inventory(unary(wire::Empty {}))
        .await
        .expect("inventory request")
        .into_inner()
        .inventory
        .map(|inventory| grpc::daemon_inventory(&inventory).expect("invalid inventory response"))
        .expect("missing inventory response");
    assert_eq!(inventory.phase, omnifs_api::DaemonPhase::Ready);
    assert_eq!(inventory.mounts.len(), 0);
    assert!(inventory.info.pid > 0);
    assert!(inventory.info.attach_tcp.is_some());
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_and_mount_survive_daemon_restart() {
    let fixture = Fixture::new();
    let provider_bytes = std::fs::read(release_wasm_dir().join("test_provider.wasm"))
        .expect("build the test provider before running acceptance tests");
    let provider_id = ProviderId::from_wasm_bytes(&provider_bytes);
    let mut daemon = fixture.start_daemon().await;
    let socket = fixture.endpoint().control_socket();
    let imported = import_provider(&socket, &provider_bytes)
        .await
        .expect("import test provider");
    assert_eq!(imported.provider.id, provider_id);

    let mut control = client(&socket).await.expect("control client");
    let mutation_id = random_mutation_id();
    begin_mutation(&mut control, mutation_id.clone()).await;
    let mount = apply_mount_create(
        &mut control,
        mutation_id,
        &mount_definition(provider_id, "durable"),
    )
    .await
    .expect("create mount");
    assert_eq!(mount.name.as_str(), "durable");

    daemon.stop().await;
    drop(daemon);
    let mut restarted = fixture.start_daemon().await;
    let inventory = wait_until_ready(&fixture.endpoint()).await;
    assert_eq!(inventory.mounts.len(), 1);
    assert_eq!(inventory.mounts[0].definition.name.as_str(), "durable");
    assert_eq!(inventory.mounts[0].definition.provider, provider_id);
    let mut control = client(&socket).await.expect("control client");
    let metadata = control
        .get_provider_metadata(unary(wire::GetProviderMetadataRequest {
            provider_id: Bytes::copy_from_slice(provider_id.as_bytes()),
        }))
        .await
        .expect("provider metadata request")
        .into_inner()
        .metadata;
    assert!(metadata.is_some());
    restarted.stop().await;
}

/// Provenance is the only recovery mechanism left: a client that lost track
/// of whether its `ApplyMutation` committed re-reads the target and compares
/// `last_mutation_id` against the id it journaled, instead of asking the
/// daemon to remember a receipt. This proves both windows converge:
/// a batch that reached the daemon stamps its row exactly once, no matter how
/// many times a confused client re-derives its outcome or retries the whole
/// logical operation from scratch, and a batch that never got its ops applied
/// (lease held, nothing written) leaves no trace to be mistaken for success.
#[tokio::test(flavor = "multi_thread")]
async fn provenance_converges_for_committed_and_not_committed_batches() {
    let fixture = Fixture::new();
    let provider_bytes = std::fs::read(release_wasm_dir().join("test_provider.wasm"))
        .expect("build the test provider before running acceptance tests");
    let provider_id = ProviderId::from_wasm_bytes(&provider_bytes);
    let mut daemon = fixture.start_daemon().await;
    let socket = fixture.endpoint().control_socket();
    import_provider(&socket, &provider_bytes)
        .await
        .expect("import test provider");

    // Window 1: a batch that committed. A client that journaled `first_id`
    // and lost the reply re-reads the mount and finds its own id stamped on
    // it, which (the batch being atomic) is exactly the "committed" signal
    // the journal's provenance check looks for.
    let mut control = client(&socket).await.expect("control client");
    let first_id = random_mutation_id();
    begin_mutation(&mut control, first_id.clone()).await;
    let created = apply_mount_create(
        &mut control,
        first_id.clone(),
        &mount_definition(provider_id, "once"),
    )
    .await
    .expect("first mount creation commits");
    assert_eq!(created.name.as_str(), "once");

    let mounts_after_first = control
        .list_mounts(unary(wire::Empty {}))
        .await
        .expect("list mounts")
        .into_inner()
        .mounts;
    assert_eq!(
        mounts_after_first.len(),
        1,
        "exactly one row after the first batch"
    );
    let stored = grpc::mount_record(&mounts_after_first[0]).expect("decode mount record");
    assert_eq!(
        stored.last_mutation_id,
        mutation_id_of(&first_id),
        "the stored row names the batch that actually wrote it"
    );

    // Re-running the same logical command (a client that never learned the
    // first attempt committed, so it retries mount creation from scratch
    // under a fresh id) must not create a duplicate row: the daemon rejects
    // the retried batch outright, and the row's provenance still names only
    // the batch that actually wrote it.
    let retry_id = random_mutation_id();
    begin_mutation(&mut control, retry_id.clone()).await;
    let retry_error = apply_mount_create(
        &mut control,
        retry_id,
        &mount_definition(provider_id, "once"),
    )
    .await
    .expect_err("retrying a completed mount creation must fail, not duplicate the row");
    assert_eq!(
        control_error_code(&retry_error),
        Some(omnifs_api::ControlErrorCode::AlreadyExists)
    );
    let mounts_after_retry = control
        .list_mounts(unary(wire::Empty {}))
        .await
        .expect("list mounts")
        .into_inner()
        .mounts;
    assert_eq!(
        mounts_after_retry.len(),
        1,
        "the failed retry must not add a second row"
    );
    let still_stored = grpc::mount_record(&mounts_after_retry[0]).expect("decode mount record");
    assert_eq!(
        still_stored.last_mutation_id, stored.last_mutation_id,
        "the row's provenance is untouched by the rejected retry"
    );

    // Window 2: a batch that never got its ops applied (the lease was
    // acquired, but the client vanished before `ApplyMutation` reached the
    // daemon). A client that journaled `stalled_id` and later re-reads its
    // target finds no row at all, which is the "not committed" signal.
    let stalled_id = random_mutation_id();
    begin_mutation(&mut control, stalled_id.clone()).await;
    control
        .drop_mutation(mutation(wire::DropMutationRequest {
            mutation_id: stalled_id,
        }))
        .await
        .expect("drop the stalled lease");
    let absent = control
        .get_mount(unary(wire::GetMountRequest {
            name: "never-created".to_owned(),
        }))
        .await
        .expect("get mount request")
        .into_inner()
        .mount;
    assert!(
        absent.is_none(),
        "a batch that never applied must leave no row for its target"
    );

    daemon.stop().await;
}

/// `status` gives whether the daemon and the host instance of a filesystem
/// run the program of the command: the instance of a record with the program
/// of `omnifs` runs it, and one of a record without a program does not.
#[tokio::test(flavor = "multi_thread")]
async fn status_gives_the_program_of_the_daemon_and_of_a_host_instance() {
    let fixture = Fixture::new();
    let location = fixture.home_path().join("mount-point");
    std::fs::create_dir_all(&location).expect("mount point");
    let location_arg = location.to_string_lossy().into_owned();
    #[cfg(target_os = "macos")]
    let protocol = "nfs";
    #[cfg(not(target_os = "macos"))]
    let protocol = "fuse";
    let create = fixture.run(&[
        "--output",
        "json",
        "fs",
        "create",
        "--name",
        "kept",
        "--protocol",
        protocol,
        "--runtime",
        "host",
        "--location",
        &location_arg,
    ]);
    assert_success(&create, "fs create");
    let mut daemon = fixture.start_daemon().await;
    let program = blake3::hash(&std::fs::read(omnifs_bin()).expect("read omnifs"))
        .to_hex()
        .to_string();
    let state_dir = fixture.home_path().join("client/filesystems/state/kept");
    std::fs::create_dir_all(&state_dir).expect("state folder");
    let record = |program: Option<&str>| {
        serde_json::json!({
            "version": 1,
            "instance_id": "0123456789abcdef0123456789abcdef",
            "pid": 42,
            "process_group": 42,
            "spec": {
                "id": "kept",
                "protocol": protocol,
                "runtime": "host",
                "location": location_arg,
            },
            "control_socket": state_dir.join("control.sock"),
            "program": program,
        })
    };
    let status = || {
        let status = fixture.run(&["--output", "json", "status"]);
        serde_json::from_slice::<serde_json::Value>(&status.stdout)
            .expect("status --output json must produce JSON")["result"]
            .clone()
    };

    std::fs::write(
        state_dir.join("runner.json"),
        record(Some(&program)).to_string(),
    )
    .expect("write the record of a host instance");
    let current = status();
    assert_eq!(current["daemon"]["current_program"], true, "{current}");
    assert_eq!(
        current["filesystems"][0]["current_program"], true,
        "{current}"
    );

    std::fs::write(state_dir.join("runner.json"), record(None).to_string())
        .expect("write the record of a host instance");
    let earlier = status();
    assert_eq!(
        earlier["filesystems"][0]["current_program"], false,
        "{earlier}"
    );
    std::fs::remove_file(state_dir.join("runner.json")).expect("remove the record");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn down_stops_daemon_but_keeps_cli_filesystem_specs() {
    let fixture = Fixture::new();
    let location = fixture.home_path().join("mount-point");
    std::fs::create_dir_all(&location).expect("mount point");
    let location_arg = location.to_string_lossy().into_owned();
    #[cfg(target_os = "macos")]
    let protocol = "nfs";
    #[cfg(not(target_os = "macos"))]
    let protocol = "fuse";
    let create = fixture.run(&[
        "--output",
        "json",
        "fs",
        "create",
        "--name",
        "kept",
        "--protocol",
        protocol,
        "--runtime",
        "host",
        "--location",
        &location_arg,
    ]);
    assert_success(&create, "fs create");
    let mut daemon = fixture.start_daemon().await;
    // Keep ownership of the daemon child while `down` runs. An exited child is
    // still visible as a zombie until this test reaps it, which proves teardown
    // uses the exact process identity rather than `kill -0` alone.
    let down = fixture.run(&["--output", "json", "down"]);
    assert_success(&down, "down");
    daemon.reap_if_exited();
    assert!(daemon.child.is_none(), "daemon child did not exit");
    let listed = fixture.run(&["--output", "json", "fs", "ls"]);
    assert_success(&listed, "fs ls");
    let json: serde_json::Value = serde_json::from_slice(&listed.stdout)
        .expect("fs ls --output json must produce valid JSON");
    let filesystems = json["result"]["filesystems"]
        .as_array()
        .expect("fs ls result.filesystems array");
    assert_eq!(filesystems.len(), 1);
    assert_eq!(filesystems[0]["id"], "kept");
    assert_eq!(filesystems[0]["state"], "detached");
    assert!(
        fixture
            .home_path()
            .join("client/filesystems/specs/kept.json")
            .is_file()
    );
}

/// `--no-input setup` boots the daemon and prints the provider catalog and
/// the `Next:` block, but neither prompt has anything to answer without a
/// terminal, so it must decline both quick-starts and still exit 0 rather
/// than mounting or attaching anything on the operator's behalf.
#[tokio::test(flavor = "multi_thread")]
async fn no_input_setup_boots_and_orients_without_mounting_or_attaching() {
    let fixture = Fixture::new();
    let setup = fixture.run(&["--no-input", "setup"]);
    assert_success(&setup, "setup");
    let stderr = String::from_utf8_lossy(&setup.stderr);
    assert!(stderr.contains("Providers you can mount"), "{stderr}");
    assert!(stderr.contains("Next:"), "{stderr}");
    assert!(stderr.contains("omnifs mount add"), "{stderr}");
    assert!(stderr.contains("omnifs status"), "{stderr}");
    assert!(stderr.contains("omnifs fs ls"), "{stderr}");

    let mounts = fixture.run(&["--output", "json", "mount", "ls"]);
    assert_success(&mounts, "mount ls");
    let mounts_json: serde_json::Value = serde_json::from_slice(&mounts.stdout)
        .expect("mount ls --output json must produce valid JSON");
    assert_eq!(
        mounts_json["result"]["mounts"]
            .as_array()
            .expect("mount ls result.mounts array")
            .len(),
        0,
        "a --no-input run must not mount anything"
    );

    let filesystems = fixture.run(&["--output", "json", "fs", "ls"]);
    assert_success(&filesystems, "fs ls");
    let filesystems_json: serde_json::Value = serde_json::from_slice(&filesystems.stdout)
        .expect("fs ls --output json must produce valid JSON");
    assert_eq!(
        filesystems_json["result"]["filesystems"]
            .as_array()
            .expect("fs ls result.filesystems array")
            .len(),
        0,
        "a --no-input run must not attach anything"
    );

    let down = fixture.run(&["down"]);
    assert_success(&down, "down");
}
