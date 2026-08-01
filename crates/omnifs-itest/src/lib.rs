pub mod live;
pub mod matrix;

use fs2::FileExt as _;
use omnifs_core::ProviderRef;
use omnifs_core::path::{Path, Segment};
use omnifs_engine::test_support::TestOp;
use omnifs_engine::{
    BuildError, Engine, EngineError, EngineNamespace, MountBuildInput, MountBuildState, MountTable,
    ProviderBuildInput, RuntimeMountConfig,
};
use omnifs_provider::{Artifact, ProviderManifest};
use omnifs_wit::host::types::{
    ByteSource, Callout, Effects, HttpRequest, ListChildrenResult, LookupChildResult,
    ReadFileOutcome, ReadFileResult,
};
use serde::Serialize;
use std::fs::OpenOptions;
use std::path::{Path as StdPath, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use tempfile::TempDir;

/// Runtime fixture for provider integration tests.
///
/// The harness owns the temporary directories that must outlive the mounted
/// provider runtime. Provider execution itself is always delegated to
/// `omnifs-engine`: tests do not build linkers, stores, or provider bindings.
pub struct RuntimeHarness {
    pub registry: Arc<MountTable>,
    pub runtime: Arc<Engine>,
    /// The single namespace owner for this immutable startup snapshot.
    pub namespace: Arc<EngineNamespace>,
    pub clone_dir: TempDir,
    pub cache_dir: TempDir,
    /// An owned executor for synchronous fixtures that have no ambient Tokio
    /// runtime. It is declared last so the namespace, registry, and temporary
    /// directories drop before the executor.
    _owned_runtime: Option<tokio::runtime::Runtime>,
}

impl RuntimeHarness {
    pub fn new(config_json: &str) -> Result<Self, BuildError> {
        Self::load_many(&[config_json], true)
    }

    pub fn new_real_callouts(config_json: &str) -> Result<Self, BuildError> {
        Self::load_many(&[config_json], false)
    }

    pub fn new_multi(configs_json: &[&str]) -> Result<Self, BuildError> {
        Self::load_many(configs_json, true)
    }

    fn load_many(configs_json: &[&str], capture_test_callouts: bool) -> Result<Self, BuildError> {
        if configs_json.is_empty() {
            return Err(BuildError::InvalidConfig(
                "integration-test harness needs at least one mount".to_string(),
            ));
        }
        let tempdir = || {
            tempfile::tempdir().map_err(|error| {
                BuildError::Cache(format!(
                    "integration-test temporary directory at {}: {error}",
                    std::env::temp_dir().display()
                ))
            })
        };
        let clone_dir = tempdir()?;
        let cache_dir = tempdir()?;
        let (handle, owned_runtime) = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            (handle, None)
        } else {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    BuildError::Cache(format!("integration-test Tokio runtime: {error}"))
                })?;
            (runtime.handle().clone(), Some(runtime))
        };

        // Validate each provider artifact once and pass its bytes and manifest
        // directly to the same durable mount builder used by the daemon. The
        // harness must not create a temporary Registry or mount-spec file just
        // to exercise an in-process runtime.
        let mounts = configs_json
            .iter()
            .map(|config_json| mount_input_from_json(config_json))
            .collect::<Result<Vec<_>, _>>()?;

        let selected_mount = mounts
            .first()
            .expect("non-empty harness specs")
            .config
            .name
            .to_string();
        let host = omnifs_engine::test_support::open_test_host(cache_dir.path(), clone_dir.path())
            .map_err(|error| BuildError::Cache(error.to_string()))?;
        let registry = if capture_test_callouts {
            omnifs_engine::test_support::load_mount_table_for_callout_tests(&host, mounts)
        } else {
            MountTable::prepare_durable(&host, mounts)
        }
        .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
        let registry = Arc::new(registry);
        let runtime = registry
            .get(&selected_mount)
            .ok_or_else(|| BuildError::InvalidConfig("test mount did not load".to_string()))?;
        let namespace = EngineNamespace::online(Arc::clone(&registry), handle);

        Ok(Self {
            registry,
            runtime,
            namespace,
            clone_dir,
            cache_dir,
            _owned_runtime: owned_runtime,
        })
    }

    pub fn lookup(
        &self,
        parent_path: &str,
        name: &str,
    ) -> Result<TestOp<'_, LookupChildResult>, EngineError> {
        self.runtime.start_lookup_child(
            &parse_path(parent_path),
            &Segment::try_from(name).expect("test lookup name must be a protocol segment"),
        )
    }

    pub fn list(&self, path: &str) -> Result<TestOp<'_, ListChildrenResult>, EngineError> {
        self.list_with_cursor(path, None)
    }

    pub fn list_with_cursor(
        &self,
        path: &str,
        cursor: Option<&omnifs_wit::host::types::Cursor>,
    ) -> Result<TestOp<'_, ListChildrenResult>, EngineError> {
        let path = parse_path(path);
        self.runtime.start_list_children(&path, cursor)
    }

    pub fn read(&self, path: &str) -> Result<TestOp<'_, ReadFileOutcome>, EngineError> {
        let path = parse_path(path);
        let content_type = path.content_type_mime(None).to_string();
        self.runtime.start_read_file(&path, &content_type, None)
    }

    pub fn timer_tick(&self) -> Result<TestOp<'_, ()>, EngineError> {
        self.runtime
            .start_event(omnifs_wit::host::types::ProviderEvent::TimerTick)
    }

    pub fn current_epoch(&self) -> u64 {
        omnifs_engine::test_support::cache::current_epoch(&self.runtime)
    }
}

pub trait TestOpExt<T> {
    fn expect_single_fetch(&self) -> &HttpRequest;
    fn expect_fetches(&self) -> Vec<&HttpRequest>;
    fn into_ok(self) -> Result<T, EngineError>;
}

impl<T> TestOpExt<T> for TestOp<'_, T> {
    fn expect_single_fetch(&self) -> &HttpRequest {
        let [Callout::Fetch(request)] = self.callouts() else {
            panic!(
                "expected exactly one fetch callout, got {:?}",
                self.callouts()
            );
        };
        request
    }

    fn expect_fetches(&self) -> Vec<&HttpRequest> {
        self.callouts()
            .iter()
            .map(|callout| match callout {
                Callout::Fetch(request) => request,
                other => panic!("expected fetch callout, got {other:?}"),
            })
            .collect()
    }

    fn into_ok(self) -> Result<T, EngineError> {
        self.into_result()?.map_err(EngineError::ProviderError)
    }
}

pub trait ReadFileOpExt {
    fn into_read_file(self) -> Result<ReadFileResult, EngineError>;
}

impl ReadFileOpExt for TestOp<'_, ReadFileOutcome> {
    fn into_read_file(self) -> Result<ReadFileResult, EngineError> {
        match self.into_result()?.map_err(EngineError::ProviderError)? {
            ReadFileOutcome::Found(result) => Ok(result),
            other @ ReadFileOutcome::NotFound(_) => Err(EngineError::ProviderProtocol(format!(
                "expected found read-file result, got {other:?}"
            ))),
        }
    }
}

/// Borrow the inline payload of a `ReadFileResult`, panicking if the
/// terminal returned a blob-backed file. Tests that intentionally
/// exercise the blob path must match on the variant directly.
pub fn expect_inline(result: &ReadFileResult) -> &[u8] {
    match &result.bytes {
        ByteSource::Inline(bytes) => bytes,
        other => panic!("expected inline file content, got {other:?}"),
    }
}

pub fn into_inline(result: ReadFileResult) -> Vec<u8> {
    match result.bytes {
        ByteSource::Inline(bytes) => bytes,
        other => panic!("expected inline file content, got {other:?}"),
    }
}

pub fn provider_artifact_dir() -> PathBuf {
    workspace_root()
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
}

pub fn provider_wasm_path(provider_name: &str) -> PathBuf {
    let path = provider_artifact_dir().join(provider_name);
    ensure_provider_artifact(&path);
    assert!(
        path.exists(),
        "{provider_name} not found at {path} after building providers.",
        path = path.display()
    );
    path
}

/// Build the provider WASM the harness loads when the requested artifact is
/// absent.
///
/// The harness loads providers as prebuilt `wasm32-wasip2` components from the
/// shared target dir. The provider build command owns source-to-WASM
/// invalidation. Test processes only enter it when the requested artifact is
/// absent, so a host test run reuses the sidecar components produced by the
/// provider job.
///
/// This runs at test *runtime*, after cargo's build phase has released the
/// target-dir lock. A filesystem lock serializes test binaries that all start
/// on a fresh checkout; the first one builds and the rest reuse its artifacts.
///
/// It delegates to `just build providers` rather than invoking cargo directly:
/// that recipe is the single source of truth for the build, including the WASI
/// SDK toolchain env (the db provider compiles `sqlite3.c` for
/// `wasm32-wasip2` through cc-rs and needs the wasi sysroot), the package
/// globs, target, and profile. Cargo decides staleness, so an up-to-date tree
/// makes this a sub-second no-op.
///
fn ensure_provider_artifact(path: &StdPath) {
    static BUILT: OnceLock<()> = OnceLock::new();
    if path.is_file() || BUILT.get().is_some() {
        return;
    }

    BUILT.get_or_init(|| {
        let target = provider_artifact_dir();
        std::fs::create_dir_all(&target).unwrap_or_else(|error| {
            panic!(
                "create provider artifact directory {}: {error}",
                target.display()
            )
        });
        let lock_path = target.join(".omnifs-provider-build.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap_or_else(|error| {
                panic!("open provider build lock {}: {error}", lock_path.display())
            });
        lock.lock_exclusive()
            .unwrap_or_else(|error| panic!("lock provider build {}: {error}", lock_path.display()));

        if path.is_file() {
            let _ = fs2::FileExt::unlock(&lock);
            return;
        }

        let status = Command::new("just")
            .args(["build", "providers"])
            .current_dir(workspace_root())
            .status()
            .expect("spawn `just build providers`");
        assert!(
            status.success(),
            "`just build providers` failed; run it directly to see the error",
        );
        let _ = fs2::FileExt::unlock(&lock);
    });
}

/// The canonical test-provider mount config the bare `make_runtime` uses.
pub const TEST_PROVIDER_CONFIG: &str = r#"{"provider":"test_provider.wasm","mount":"test"}"#;

pub fn make_runtime() -> RuntimeHarness {
    RuntimeHarness::new(TEST_PROVIDER_CONFIG).unwrap()
}

pub fn try_make_runtime_from_config(
    config_json: &str,
) -> Result<RuntimeHarness, omnifs_engine::BuildError> {
    RuntimeHarness::new(config_json)
}

pub fn make_initialized_runtime(config_json: &str) -> RuntimeHarness {
    RuntimeHarness::new(config_json).unwrap()
}

/// Build the daemon-facing durable input for one JSON test mount.
///
/// This keeps the test helper's public JSON convenience while making the
/// runtime boundary explicit: provider bytes, validated metadata, canonical
/// mount bytes, and the state-neutral runtime config all travel together.
pub fn mount_input_from_json(config_json: &str) -> Result<MountBuildInput, BuildError> {
    let value: serde_json::Value = serde_json::from_str(config_json)
        .map_err(|error| BuildError::InvalidConfig(format!("parse test config: {error}")))?;
    let object = value.as_object().ok_or_else(|| {
        BuildError::InvalidConfig("test config must be a JSON object".to_string())
    })?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "provider" | "mount" | "auth" | "limits" | "config"
        ) {
            return Err(BuildError::InvalidConfig(format!(
                "test config has unknown field `{key}`"
            )));
        }
    }
    let provider_file = object
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| BuildError::InvalidConfig("test config has no string `provider`".into()))?;
    let mount = object
        .get("mount")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| BuildError::InvalidConfig("test config has no string `mount`".into()))?;
    let name = omnifs_core::ResourceName::new(mount.to_owned())
        .map_err(|error| BuildError::InvalidConfig(format!("invalid mount `{mount}`: {error}")))?;

    let (reference, bytes, manifest) = pin_provider(provider_file)?;
    let config = object
        .get("config")
        .cloned()
        .or_else(|| {
            manifest
                .config
                .as_ref()
                .map(omnifs_provider::ConfigMetadata::defaults)
        })
        .unwrap_or_else(|| serde_json::json!({}));
    let max_fetch_blob_bytes = object
        .get("limits")
        .and_then(serde_json::Value::as_object)
        .and_then(|limits| limits.get("max_fetch_blob_bytes"))
        .map(|value| {
            value.as_u64().ok_or_else(|| {
                BuildError::InvalidConfig(
                    "limits.max_fetch_blob_bytes must be an unsigned integer".to_string(),
                )
            })
        })
        .transpose()?;
    let canonical = canonical_mount_bytes(
        &reference,
        mount,
        object.get("auth").cloned(),
        object.get("limits").cloned(),
        object.get("config").cloned().or_else(|| {
            manifest
                .config
                .as_ref()
                .map(omnifs_provider::ConfigMetadata::defaults)
        }),
    )?;

    Ok(MountBuildInput {
        config: RuntimeMountConfig {
            name,
            provider: reference,
            config,
            max_fetch_blob_bytes,
        },
        canonical,
        provider: Some(ProviderBuildInput { bytes, manifest }),
        state: MountBuildState::Active {
            // Auth bindings are daemon-owned. These provider integration tests
            // use canned callouts and do not inject credentials.
            auth: None,
            credential_generation: None,
        },
    })
}

/// Read and validate a built provider artifact. The returned bytes remain in
/// the harness so the engine can build directly from this validated input.
fn pin_provider(
    provider_file: &str,
) -> Result<(ProviderRef, Arc<[u8]>, ProviderManifest), BuildError> {
    let src = provider_wasm_path(provider_file);
    let bytes = std::fs::read(&src)
        .map_err(|error| BuildError::InvalidConfig(format!("read {}: {error}", src.display())))?;
    let (artifact, manifest) = Artifact::from_bytes_with_manifest(provider_file, bytes.clone())
        .map_err(|error| BuildError::InvalidConfig(format!("{provider_file}: {error}")))?;
    Ok((
        artifact.reference(),
        Arc::from(bytes.into_boxed_slice()),
        manifest,
    ))
}

#[derive(Serialize)]
struct CanonicalMount<'a> {
    provider: ProviderRef,
    mount: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limits: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<serde_json::Value>,
}

fn canonical_mount_bytes(
    provider: &ProviderRef,
    mount: &str,
    auth: Option<serde_json::Value>,
    limits: Option<serde_json::Value>,
    config: Option<serde_json::Value>,
) -> Result<Arc<[u8]>, BuildError> {
    let mut bytes = serde_json::to_vec_pretty(&CanonicalMount {
        provider: provider.clone(),
        mount,
        auth,
        limits,
        config,
    })
    .map_err(|error| BuildError::InvalidConfig(format!("serialize test mount: {error}")))?;
    bytes.push(b'\n');
    Ok(Arc::from(bytes.into_boxed_slice()))
}

pub fn project_paths(effects: &Effects) -> Vec<&str> {
    effects.fs.iter().map(|write| write.path.as_str()).collect()
}

pub(crate) fn workspace_root() -> PathBuf {
    StdPath::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

pub fn parse_path(path: &str) -> Path {
    Path::parse(path).unwrap_or_else(|error| panic!("test path must be absolute: {path}: {error}"))
}

/// Initialises a git repo in `dir` with a README and a src/main.rs, then
/// commits them. Used by tests that need a real local repo for the git
/// executor or for seeding the clone cache. The README content is caller-
/// supplied so tests can assert on it.
pub fn create_test_repo(dir: &StdPath, readme_content: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "-b", "main"]);
    std::fs::write(dir.join("README.md"), readme_content).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "init"]);
}
