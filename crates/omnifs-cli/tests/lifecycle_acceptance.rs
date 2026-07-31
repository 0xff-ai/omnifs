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
    API_VERSION, ApplyReceipt, ApplyResourcesRequest, CONTROL_REQUEST_TIMEOUT_SECS,
    CONTROL_STREAM_PAYLOAD_MAX_BYTES, DaemonInventory, FilesystemDefinition, FilesystemPhase,
    MountResourceDefinition, ProviderDefinition, ResourceDeclarations, ResourceDefinition,
    ResourcePhase, ResourceSnapshot,
};
use omnifs_bootstrap::Profile;
use omnifs_core::{
    FilesystemProtocol, FilesystemRuntime, FilesystemSpec, MutationId, ProviderId, ResourceKind,
    ResourceName,
};
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
const PROVIDER_CHUNK_BYTES: usize = CONTROL_STREAM_PAYLOAD_MAX_BYTES;

fn unary<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.set_timeout(REQUEST_TIMEOUT);
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

    fn endpoint(&self) -> Profile {
        Profile::under_root(self.home_path())
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

    async fn start_daemon(&self) -> DaemonGuard {
        let child = Command::new(omnifs_bin())
            .arg("daemon")
            .env("OMNIFS_HOME", self.home_path())
            .env("RUST_LOG", "warn")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn omnifs daemon");
        let mut guard = DaemonGuard {
            child: Some(child),
            endpoint: self.endpoint(),
        };
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
    endpoint: Profile,
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

async fn wait_until_ready(endpoint: &Profile) -> DaemonInventory {
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

fn random_mutation_id() -> MutationId {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("generate mutation id");
    MutationId::from_bytes(bytes)
}

async fn resource_snapshot(control: &mut ControlClient) -> ResourceSnapshot {
    let response = control
        .get_resources(unary(wire::Empty {}))
        .await
        .expect("get resources")
        .into_inner();
    grpc::get_resources_response(&response).expect("decode resource snapshot")
}

async fn resource_plan(
    control: &mut ControlClient,
    resources: Vec<ResourceDefinition>,
) -> (ResourceDeclarations, omnifs_api::ResourcePlan) {
    let declarations = ResourceDeclarations {
        api_version: API_VERSION.to_owned(),
        resources,
    };
    let response = control
        .plan_resources(unary(grpc::to_plan_resources_request(&declarations)))
        .await
        .expect("plan resources")
        .into_inner();
    let plan = grpc::plan_resources_response(&response).expect("decode resource plan");
    (declarations, plan)
}

async fn apply_resource_plan(
    control: &mut ControlClient,
    mutation_id: MutationId,
    declarations: ResourceDeclarations,
    plan: &omnifs_api::ResourcePlan,
) -> ApplyReceipt {
    let response = control
        .apply_resources(unary(grpc::to_apply_resources_request(
            &ApplyResourcesRequest {
                mutation_id,
                base_revision: plan.base_revision,
                expected_desired_digest: plan.desired_digest,
                declarations,
                credential_material: Vec::new(),
            },
        )))
        .await
        .expect("apply resources")
        .into_inner();
    grpc::apply_resources_response(&response).expect("decode apply receipt")
}

async fn apply_resources(
    control: &mut ControlClient,
    resources: Vec<ResourceDefinition>,
) -> ApplyReceipt {
    let (declarations, plan) = resource_plan(control, resources).await;
    apply_resource_plan(control, random_mutation_id(), declarations, &plan).await
}

fn provider_and_mount(
    provider: ProviderId,
    name: &str,
) -> (ResourceDefinition, ResourceDefinition) {
    let resource_name = ResourceName::new(name).expect("valid resource name");
    (
        ResourceDefinition::Provider(ProviderDefinition {
            name: resource_name.clone(),
            artifact: provider,
        }),
        ResourceDefinition::Mount(MountResourceDefinition {
            name: resource_name.clone(),
            provider: resource_name,
            credential: None,
            config: serde_json::json!({}),
            limits: None,
        }),
    )
}

async fn wait_for_resource_ready(
    control: &mut ControlClient,
    kind: ResourceKind,
    name: &ResourceName,
) -> ResourceSnapshot {
    let deadline = tokio::time::Instant::now() + PROVIDER_IMPORT_TIMEOUT;
    loop {
        let snapshot = resource_snapshot(control).await;
        let status = snapshot
            .resource_statuses
            .iter()
            .find(|status| status.key.kind == kind && status.key.name == *name);
        match status {
            Some(status) if status.phase == ResourcePhase::Ready => return snapshot,
            Some(status)
                if matches!(status.phase, ResourcePhase::Failed | ResourcePhase::Blocked) =>
            {
                panic!("resource {kind}/{name} failed: {:?}", status.detail)
            },
            _ => {},
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "resource {kind}/{name} did not become ready"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn filesystem_definition(name: &str, location: &Path) -> FilesystemDefinition {
    #[cfg(target_os = "macos")]
    let protocol = FilesystemProtocol::Nfs;
    #[cfg(not(target_os = "macos"))]
    let protocol = FilesystemProtocol::Fuse;
    FilesystemDefinition {
        name: ResourceName::new(name).expect("valid Filesystem name"),
        spec: FilesystemSpec::new(
            protocol,
            FilesystemRuntime::Host,
            location.to_owned(),
            None,
            None,
        )
        .expect("valid host Filesystem spec"),
    }
}

async fn wait_for_filesystem_ready(
    control: &mut ControlClient,
    name: &ResourceName,
) -> omnifs_api::FilesystemStatus {
    let deadline = tokio::time::Instant::now() + PROVIDER_IMPORT_TIMEOUT;
    loop {
        let response = control
            .get_filesystem_status(unary(wire::GetFilesystemStatusRequest {
                filesystem_name: name.to_string(),
            }))
            .await
            .expect("get Filesystem status")
            .into_inner();
        if let Some(status) =
            grpc::get_filesystem_status_response(&response).expect("decode Filesystem status")
        {
            match status.phase {
                FilesystemPhase::Ready => return status,
                FilesystemPhase::Failed => {
                    panic!("Filesystem {name} failed: {:?}", status.detail)
                },
                _ => {},
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Filesystem {name} did not become ready"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
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
    let (provider, mount) = provider_and_mount(provider_id, "durable");
    let receipt = apply_resources(&mut control, vec![provider, mount]).await;
    assert!(receipt.changed);
    let name = ResourceName::new("durable").expect("valid resource name");
    wait_for_resource_ready(&mut control, ResourceKind::Mount, &name).await;

    daemon.stop().await;
    drop(daemon);
    let mut restarted = fixture.start_daemon().await;
    let mut control = client(&socket).await.expect("control client");
    let snapshot = wait_for_resource_ready(&mut control, ResourceKind::Mount, &name).await;
    assert_eq!(snapshot.revision, receipt.revision);
    assert_eq!(snapshot.serving_revision, Some(receipt.revision));
    assert!(snapshot.resources.iter().any(|resource| matches!(
        resource,
        ResourceDefinition::Provider(definition)
            if definition.name == name && definition.artifact == provider_id
    )));
    assert!(snapshot.resources.iter().any(|resource| matches!(
        resource,
        ResourceDefinition::Mount(definition)
            if definition.name == name && definition.provider == name
    )));
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

/// A desired-set apply keeps a durable receipt keyed by the client mutation
/// ID. Repeating the exact request after a lost reply returns that receipt,
/// while a new ID for the already-current set returns an unchanged receipt.
#[tokio::test(flavor = "multi_thread")]
async fn declarative_apply_receipts_converge_after_lost_replies() {
    let fixture = Fixture::new();
    let provider_bytes = std::fs::read(release_wasm_dir().join("test_provider.wasm"))
        .expect("build the test provider before running acceptance tests");
    let provider_id = ProviderId::from_wasm_bytes(&provider_bytes);
    let mut daemon = fixture.start_daemon().await;
    let socket = fixture.endpoint().control_socket();
    import_provider(&socket, &provider_bytes)
        .await
        .expect("import test provider");

    let mut control = client(&socket).await.expect("control client");
    let (provider, mount) = provider_and_mount(provider_id, "once");
    let resources = vec![provider, mount];
    let (declarations, plan) = resource_plan(&mut control, resources.clone()).await;
    let first_id = random_mutation_id();
    let first = apply_resource_plan(&mut control, first_id, declarations, &plan).await;
    assert!(first.changed);
    assert_eq!(first.mutation_id, first_id);

    let replay = apply_resource_plan(
        &mut control,
        first_id,
        ResourceDeclarations {
            api_version: API_VERSION.to_owned(),
            resources: resources.clone(),
        },
        &plan,
    )
    .await;
    assert_eq!(
        replay, first,
        "lost-reply retry returns the durable receipt"
    );

    let (declarations, unchanged_plan) = resource_plan(&mut control, resources).await;
    let unchanged = apply_resource_plan(
        &mut control,
        random_mutation_id(),
        declarations,
        &unchanged_plan,
    )
    .await;
    assert!(!unchanged.changed);
    assert_eq!(unchanged.revision, first.revision);
    let snapshot = resource_snapshot(&mut control).await;
    assert_eq!(snapshot.resources.len(), 2);

    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn down_stops_runtime_but_preserves_desired_filesystem_across_restart() {
    let fixture = Fixture::new();
    let location = fixture.home_path().join("mount-point");
    std::fs::create_dir_all(&location).expect("mount point");
    let mut daemon = fixture.start_daemon().await;
    let socket = fixture.endpoint().control_socket();
    let mut control = client(&socket).await.expect("control client");
    let filesystem = filesystem_definition("kept", &location);
    let receipt = apply_resources(
        &mut control,
        vec![ResourceDefinition::Filesystem(filesystem.clone())],
    )
    .await;
    assert!(receipt.changed);
    let status = wait_for_filesystem_ready(&mut control, &filesystem.name).await;
    assert_eq!(status.desired_revision, receipt.revision);
    assert!(
        !fixture
            .home_path()
            .join("client/filesystems/specs/kept.json")
            .exists(),
        "normal lifecycle must not write a client filesystem spec"
    );
    // Keep ownership of the daemon child while `down` runs. An exited child is
    // still visible as a zombie until this test reaps it, which proves teardown
    // uses the exact process identity rather than `kill -0` alone.
    let down = fixture.run(&["--output", "json", "down"]);
    assert_success(&down, "down");
    daemon.reap_if_exited();
    assert!(daemon.child.is_none(), "daemon child did not exit");
    assert!(!fixture.home_path().join("client/filesystems").exists());

    let mut restarted = fixture.start_daemon().await;
    let mut control = client(&socket).await.expect("control client");
    let restored = wait_for_filesystem_ready(&mut control, &filesystem.name).await;
    assert_eq!(restored.desired_revision, receipt.revision);
    let listed = fixture.run(&["--output", "json", "fs", "ls"]);
    assert_success(&listed, "fs ls");
    let json: serde_json::Value = serde_json::from_slice(&listed.stdout)
        .expect("fs ls --output json must produce valid JSON");
    let filesystems = json["result"]["filesystems"]
        .as_array()
        .expect("fs ls result.filesystems array");
    assert_eq!(filesystems.len(), 1);
    assert_eq!(filesystems[0]["name"], "kept");
    assert_eq!(filesystems[0]["phase"], "ready");

    assert!(!fixture.home_path().join("client/filesystems").exists());
    restarted.stop().await;
}

/// `--no-input setup` may orient and inspect the current desired set, but it
/// declines both quick-start offers and never applies state on the operator's
/// behalf.
#[tokio::test(flavor = "multi_thread")]
async fn no_input_setup_boots_and_orients_without_mounting_or_attaching() {
    let fixture = Fixture::new();
    let setup = fixture.run(&["--no-input", "setup"]);
    assert_success(&setup, "setup");
    let stderr = String::from_utf8_lossy(&setup.stderr);
    assert!(stderr.contains("Providers you can mount"), "{stderr}");
    assert!(!stderr.contains("Apply setup plan?"), "{stderr}");

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
