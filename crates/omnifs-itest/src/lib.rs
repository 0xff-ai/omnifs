pub mod live;
pub mod matrix;

use omnifs_core::path::{Path, Segment};
use omnifs_engine::GitCloner;
use omnifs_engine::test_support::TestOp;
use omnifs_engine::test_support::cache::{Record as CacheRecord, RecordKind};
use omnifs_engine::{BuildError, Engine, EngineError, HostContext, MountRuntimes, TreeNamespace};
use omnifs_wit::provider::types::{
    ByteSource, Callout, Effects, HttpRequest, ListChildrenResult, LookupChildResult,
    ReadFileOutcome, ReadFileResult,
};
use omnifs_workspace::ids::{ProviderId, ProviderMeta, ProviderName, ProviderRef};
use omnifs_workspace::mounts::Spec;
use omnifs_workspace::provider::{Artifact, Catalog, ProviderStore};
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
    pub registry: Arc<MountRuntimes>,
    pub runtime: Arc<Engine>,
    /// The single namespace owner for this immutable startup snapshot.
    pub namespace: Arc<TreeNamespace>,
    pub clone_dir: TempDir,
    pub cache_dir: TempDir,
    pub config_dir: TempDir,
    /// Per-harness content-addressed provider store the runtime resolves from.
    pub providers_dir: TempDir,
    pub mounts_dir: TempDir,
    /// An owned executor for synchronous fixtures that have no ambient Tokio
    /// runtime. It is declared last so the namespace, registry, and temporary
    /// directories drop before the executor.
    owned_runtime: Option<tokio::runtime::Runtime>,
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
        let config_dir = tempdir()?;
        let providers_dir = tempdir()?;
        let mounts_dir = tempdir()?;
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());
        let (handle, owned_runtime) = match tokio::runtime::Handle::try_current() {
            Ok(handle) => (handle, None),
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        BuildError::Cache(format!("integration-test Tokio runtime: {error}"))
                    })?;
                (runtime.handle().clone(), Some(runtime))
            },
        };

        // Pin the named provider into this harness's provider store and rewrite
        // the test config's `provider` field to the resulting `ProviderRef`, so
        // resolution and serving go through the content-addressed path the host
        // uses in production.
        let mut specs = configs_json
            .iter()
            .map(|config_json| pin_spec_from_json(config_json, providers_dir.path()))
            .collect::<Result<Vec<_>, _>>()?;

        // Mirror the CLI's creation-time inheritance: bake the pinned provider's
        // manifest defaults into the spec before serving, so the harness exercises
        // the same already-hydrated spec the daemon sees in production.
        let catalog = Catalog::open(providers_dir.path());
        for spec in &mut specs {
            let provider = catalog
                .get(&spec.provider.id)
                .map_err(|error| BuildError::InvalidConfig(error.to_string()))?
                .ok_or_else(|| BuildError::InvalidConfig("pinned provider missing".to_string()))?;
            let manifest = provider
                .manifest()
                .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
            spec.apply_provider_metadata(
                &manifest,
                omnifs_workspace::mounts::ProviderMetadataInheritance::all(),
            )
            .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
        }
        let selected_mount = specs
            .first()
            .expect("non-empty harness specs")
            .mount
            .clone();
        let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()).map_err(
            |source| {
                BuildError::Cache(format!(
                    "git clone cache at {}: {source}",
                    clone_dir.path().display()
                ))
            },
        )?);
        let context = HostContext::new(
            cache_dir.path(),
            &paths.config_dir,
            providers_dir.path(),
            &paths.credentials_file,
        );
        let mut desired = omnifs_workspace::mounts::Registry::load(mounts_dir.path())
            .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
        for spec in &specs {
            desired
                .put(spec)
                .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
        }
        let registry = if capture_test_callouts {
            omnifs_engine::test_support::load_mount_runtimes_for_callout_tests(
                context, cloner, &desired, &handle,
            )
        } else {
            MountRuntimes::load(context, cloner, &desired, &handle)
        }
        .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
        let registry = Arc::new(registry);
        let runtime = registry
            .get(&selected_mount)
            .ok_or_else(|| BuildError::InvalidConfig("test mount did not load".to_string()))?;
        let namespace = TreeNamespace::new(Arc::clone(&registry), handle);

        Ok(Self {
            clone_dir,
            cache_dir,
            config_dir,
            providers_dir,
            mounts_dir,
            registry,
            runtime,
            namespace,
            owned_runtime,
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
        cursor: Option<&omnifs_wit::provider::types::Cursor>,
    ) -> Result<TestOp<'_, ListChildrenResult>, EngineError> {
        let path = parse_path(path);
        self.runtime.start_list_children(&path, None, cursor)
    }

    pub fn read(&self, path: &str) -> Result<TestOp<'_, ReadFileOutcome>, EngineError> {
        let path = parse_path(path);
        let content_type = path.content_type_mime(None).to_string();
        self.runtime.start_read_file(&path, &content_type, None)
    }

    pub fn timer_tick(&self) -> Result<TestOp<'_, ()>, EngineError> {
        self.runtime
            .start_event(omnifs_wit::provider::types::ProviderEvent::TimerTick)
    }

    pub fn cache_get(
        &self,
        path: &str,
        kind: RecordKind,
        aux: Option<&str>,
    ) -> Option<CacheRecord> {
        omnifs_engine::test_support::cache::cache_get(&self.runtime, &parse_path(path), kind, aux)
    }

    pub fn cached_canonical_for(
        &self,
        path: &str,
    ) -> Option<omnifs_engine::test_support::cache::CachedCanonical> {
        omnifs_engine::test_support::cache::cached_canonical_for(&self.runtime, &parse_path(path))
    }

    pub fn current_generation(&self) -> u64 {
        omnifs_engine::test_support::cache::current_generation(&self.runtime)
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
    ensure_providers_built();
    let path = provider_artifact_dir().join(provider_name);
    assert!(
        path.exists(),
        "{provider_name} not found at {path} after building providers.",
        path = path.display()
    );
    path
}

/// Build (or refresh) the provider WASM the harness loads.
///
/// The harness loads providers as prebuilt `wasm32-wasip2` components from the
/// shared target dir. Running tests against a stale build silently exercises
/// old provider logic (a pagination change that never took effect, say), which
/// surfaces as a confusing test failure unrelated to the edit in hand. Rather
/// than require a manual `just build providers`, refresh the components on
/// demand.
///
/// This runs at test *runtime*, after cargo's build phase has released the
/// target-dir lock, so the build it triggers can write into the same
/// `target/wasm32-wasip2/release` that the test binary loads from (cache reused,
/// no second build tree) without deadlocking against the build that produced
/// this test binary.
///
/// It delegates to `just build providers` rather than invoking cargo directly:
/// that recipe is the single source of truth for the build, including the WASI
/// SDK toolchain env (the db provider compiles `sqlite3.c` for
/// `wasm32-wasip2` through cc-rs and needs the wasi sysroot), the package
/// globs, target, and profile. Cargo decides staleness, so an up-to-date tree
/// makes this a sub-second no-op.
///
/// Set `OMNIFS_ITEST_SKIP_PROVIDER_BUILD=1` to skip it (e.g. CI, which builds
/// the provider wasm in a separate job and hands it to the test job as an
/// artifact, with no wasm toolchain on the test runner).
fn ensure_providers_built() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        if std::env::var_os("OMNIFS_ITEST_SKIP_PROVIDER_BUILD").is_some() {
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

/// Pin the provider named in `config_json`'s `provider` field into the provider
/// store under `providers_dir`, then return the config as a `Spec` whose
/// `provider` is the resulting `ProviderRef`. This routes test resolution and
/// serving through the content-addressed path the host uses in production.
fn pin_spec_from_json(config_json: &str, providers_dir: &StdPath) -> Result<Spec, BuildError> {
    let mut value: serde_json::Value = serde_json::from_str(config_json)
        .map_err(|error| BuildError::InvalidConfig(format!("parse test config: {error}")))?;
    let provider_file = value
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| BuildError::InvalidConfig("test config has no string `provider`".into()))?
        .to_string();
    let reference = pin_provider(providers_dir, &provider_file)?;
    value["provider"] = serde_json::to_value(&reference)
        .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
    serde_json::from_value(value)
        .map_err(|error| BuildError::InvalidConfig(format!("build test spec: {error}")))
}

/// Lay the built `provider_file` WASM into the provider store and return its pinned
/// reference, named from the artifact's embedded manifest id (which can differ
/// from the file stem, e.g. `test_provider.wasm` -> `test-provider`).
fn pin_provider(providers_dir: &StdPath, provider_file: &str) -> Result<ProviderRef, BuildError> {
    let src = provider_wasm_path(provider_file);
    let bytes = std::fs::read(&src)
        .map_err(|error| BuildError::InvalidConfig(format!("read {}: {error}", src.display())))?;
    let artifact = Artifact::from_bytes(provider_file, bytes)
        .map_err(|error| BuildError::InvalidConfig(format!("{provider_file}: {error}")))?;
    let reference = artifact.reference();
    let store = ProviderStore::new(providers_dir);
    store
        .retain(&artifact)
        .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
    Ok(reference)
}

/// A pinned reference for the test provider with a placeholder id, for tests
/// that pass the wasm path to `Runtime::new` directly and never resolve through
/// a store.
#[must_use]
pub fn test_provider_ref() -> ProviderRef {
    ProviderRef {
        id: ProviderId::from_wasm_bytes(b"test-provider"),
        meta: ProviderMeta {
            name: ProviderName::new("test-provider").unwrap(),
            version: None,
        },
    }
}

/// Build a `Spec` from a JSON `body` (with no `provider` field) plus the test
/// provider's placeholder reference. For tests that drive `Runtime::new`
/// directly with a known wasm path rather than through the store.
#[must_use]
pub fn spec_with_test_provider(body: &str) -> Spec {
    let mut value: serde_json::Value = serde_json::from_str(body).expect("test body json");
    value["provider"] = serde_json::to_value(test_provider_ref()).expect("serialize provider ref");
    serde_json::from_value(value).expect("build test spec")
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
