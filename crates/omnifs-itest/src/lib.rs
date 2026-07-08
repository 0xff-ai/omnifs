pub mod live;
pub mod matrix;
pub mod scenario;
pub mod tape;

use omnifs_core::path::{Path, Segment};
use omnifs_engine::GitCloner;
use omnifs_engine::test_support::cache::{Caches, Record as CacheRecord, RecordKind};
use omnifs_engine::test_support::{ObjectId, Op, TestOp};
use omnifs_engine::{
    BuildError, Engine, EngineError, HostContext, RequestCtx, ServingContext, Tree,
};
use omnifs_wit::provider::types::{
    ByteSource, Callout, CanonicalInput, Effects, HttpRequest, ListChildrenResult,
    LookupChildResult, OpResult, ReadFileOutcome, ReadFileResult,
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
    pub engine: wasmtime::Engine,
    pub clone_dir: TempDir,
    pub cache_dir: TempDir,
    pub config_dir: TempDir,
    /// Per-harness content-addressed provider store the runtime resolves from.
    pub providers_dir: TempDir,
    pub runtime: Engine,
}

/// How a harness answers provider callouts.
pub enum CalloutSetup {
    /// Fetch/FetchBlob captured for hand-answering or tape replay. The default.
    Captured,
    /// Real executors (network). Used by domain-denial tests and record mode
    /// without a recorder (not normally needed).
    Real,
    /// Real executors plus a tape recorder tee observing every callout.
    Recorded(Arc<crate::tape::record::TapeRecorder>),
}

/// A pre-construction hook run against the workspace layout before the engine
/// is built. See [`HarnessBuilder::prepare`].
type PrepareHook<'a> = Box<dyn FnOnce(&omnifs_workspace::layout::WorkspaceLayout) + 'a>;

/// Builder for a [`RuntimeHarness`]: selects the wasmtime engine, the callout
/// wiring, and an optional pre-construction hook.
pub struct HarnessBuilder<'a> {
    config_json: &'a str,
    engine: Option<&'a wasmtime::Engine>,
    callouts: CalloutSetup,
    /// Runs after the workspace layout exists on disk and before engine
    /// construction. Record mode uses it to write credentials into
    /// `paths.credentials_file`.
    prepare: Option<PrepareHook<'a>>,
}

impl RuntimeHarness {
    /// Start building a harness for `config_json`. Defaults to a shared memoized
    /// engine and captured callouts.
    pub fn builder(config_json: &str) -> HarnessBuilder<'_> {
        HarnessBuilder {
            config_json,
            engine: None,
            callouts: CalloutSetup::Captured,
            prepare: None,
        }
    }

    /// Build a captured-callout harness from `config_json`. Sugar for
    /// `builder(config_json).build()`.
    pub fn new(config_json: &str) -> Result<Self, BuildError> {
        Self::builder(config_json).build()
    }

    pub fn start_op(&self, op: Op) -> Result<TestOp<'_>, EngineError> {
        self.runtime.start_op(op)
    }

    pub fn lookup(&self, parent_path: &str, name: &str) -> Result<TestOp<'_>, EngineError> {
        self.start_op(Op::LookupChild {
            parent_path: parse_path(parent_path),
            name: Segment::try_from(name).expect("test lookup name must be a protocol segment"),
        })
    }

    pub fn list(&self, path: &str) -> Result<TestOp<'_>, EngineError> {
        self.list_with_cursor(path, None)
    }

    pub fn list_with_cursor(
        &self,
        path: &str,
        cursor: Option<omnifs_wit::provider::types::Cursor>,
    ) -> Result<TestOp<'_>, EngineError> {
        self.start_op(Op::ListChildren {
            path: parse_path(path),
            cached_validator: None,
            cursor,
        })
    }

    pub fn read(&self, path: &str) -> Result<TestOp<'_>, EngineError> {
        let path = parse_path(path);
        self.start_op(Op::ReadFile {
            content_type: path.content_type_mime(None).to_string(),
            path,
            cached_canonical: None,
        })
    }

    /// Start a revalidating read: push the path's cached canonical back to the
    /// provider with `revalidate: true`, the exact op shape the engine's
    /// background revalidation timer sends through `revalidate_file`, so the
    /// provider issues a conditional fetch against the stored validator.
    ///
    /// # Panics
    ///
    /// Panics when the path has no cached canonical: a revalidation only makes
    /// sense after a prior step warmed the object cache.
    pub fn revalidate(&self, path: &str) -> Result<TestOp<'_>, EngineError> {
        let parsed = parse_path(path);
        let cached = self
            .runtime
            .cache()
            .cached_canonical_for(&parsed)
            .unwrap_or_else(|| {
                panic!("revalidate({path}) requires a prior step to cache the object canonical")
            });
        let id = ObjectId::from_bytes(cached.id)
            .to_wit()
            .expect("cached canonical id decodes to a wit logical id");
        self.start_op(Op::ReadFile {
            content_type: parsed.content_type_mime(None).to_string(),
            path: parsed,
            cached_canonical: Some(CanonicalInput {
                id,
                validator: cached.validator,
                bytes: cached.bytes,
                revalidate: true,
            }),
        })
    }

    pub fn timer_tick(&self) -> Result<TestOp<'_>, EngineError> {
        self.start_op(Op::OnEvent {
            event: omnifs_wit::provider::types::ProviderEvent::TimerTick,
        })
    }

    pub fn cache_get(
        &self,
        path: &str,
        kind: RecordKind,
        aux: Option<&str>,
    ) -> Option<CacheRecord> {
        self.runtime.cache().cache_get(&parse_path(path), kind, aux)
    }

    pub fn cached_canonical_for(
        &self,
        path: &str,
    ) -> Option<omnifs_engine::test_support::cache::CachedCanonical> {
        self.runtime.cache().cached_canonical_for(&parse_path(path))
    }

    pub fn current_generation(&self) -> u64 {
        self.runtime.cache().current_generation()
    }
}

/// A wasm test-provider loaded into a `Runtime` and wrapped in a `Tree` under
/// mount "test". Owns the harness temp dirs that must outlive the `Runtime`, plus
/// an `Arc<Engine>` clone so a suite can drive object invalidations directly. The
/// RAII owner shared by the tree-conformance, pagination, and live-growth suites.
pub struct TreeHarness {
    pub tree: Tree,
    pub runtime: Arc<Engine>,
    pub ctx: RequestCtx,
    _clone_dir: TempDir,
    _cache_dir: TempDir,
    _config_dir: TempDir,
}

/// Load `test_provider.wasm` into a `Runtime` and build a `Tree` over it under
/// mount "test".
pub fn tree_harness() -> TreeHarness {
    let engine = make_engine();
    let RuntimeHarness {
        clone_dir,
        cache_dir,
        config_dir,
        runtime,
        ..
    } = make_runtime(&engine);
    let runtime = Arc::new(runtime);
    let tree = Tree::new(ServingContext::single(
        "test".to_string(),
        Arc::clone(&runtime),
    ));
    TreeHarness {
        tree,
        runtime,
        ctx: RequestCtx::default(),
        _clone_dir: clone_dir,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
    }
}

impl<'a> HarnessBuilder<'a> {
    /// Use `engine` instead of the shared memoized engine.
    #[must_use]
    pub fn engine(mut self, engine: &'a wasmtime::Engine) -> Self {
        self.engine = Some(engine);
        self
    }

    /// Select how provider callouts are wired.
    #[must_use]
    pub fn callouts(mut self, setup: CalloutSetup) -> Self {
        self.callouts = setup;
        self
    }

    /// Run `f` against the workspace layout after it exists on disk and before
    /// the engine is constructed.
    #[must_use]
    pub fn prepare(
        mut self,
        f: impl FnOnce(&omnifs_workspace::layout::WorkspaceLayout) + 'a,
    ) -> Self {
        self.prepare = Some(Box::new(f));
        self
    }

    #[allow(clippy::too_many_lines)]
    pub fn build(self) -> Result<RuntimeHarness, BuildError> {
        let HarnessBuilder {
            config_json,
            engine,
            callouts,
            prepare,
        } = self;

        // Default to the shared memoized engine when the caller did not pin one.
        // The owned binding keeps the default alive for the borrow below.
        let owned_engine;
        let engine: &wasmtime::Engine = if let Some(engine) = engine {
            engine
        } else {
            owned_engine = make_engine();
            &owned_engine
        };

        let clone_dir = tempfile::tempdir().map_err(|source| BuildError::CacheDir {
            path: std::env::temp_dir(),
            source,
        })?;
        let cache_dir = tempfile::tempdir().map_err(|source| BuildError::CacheDir {
            path: std::env::temp_dir(),
            source,
        })?;
        let config_dir = tempfile::tempdir().map_err(|source| BuildError::CacheDir {
            path: std::env::temp_dir(),
            source,
        })?;
        let providers_dir = tempfile::tempdir().map_err(|source| BuildError::CacheDir {
            path: std::env::temp_dir(),
            source,
        })?;
        let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());

        // The prepare hook seeds on-disk state (record mode writes credentials
        // into paths.credentials_file) now that the layout exists but before the
        // engine reads it during construction.
        if let Some(prepare) = prepare {
            prepare(&paths);
        }

        // Pin the named provider into this harness's provider store and rewrite
        // the test config's `provider` field to the resulting `ProviderRef`, so
        // resolution and serving go through the content-addressed path the host
        // uses in production.
        let mut spec = pin_spec_from_json(config_json, providers_dir.path())?;

        // Mirror the CLI's creation-time inheritance: bake the pinned provider's
        // manifest defaults into the spec before serving, so the harness exercises
        // the same already-hydrated spec the daemon sees in production.
        let catalog = Catalog::open(providers_dir.path());
        if let Some(provider) = catalog
            .get(&spec.provider.id)
            .map_err(|error| BuildError::InvalidConfig(error.to_string()))?
        {
            let manifest = provider
                .manifest()
                .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
            spec.apply_provider_metadata(
                &manifest,
                omnifs_workspace::mounts::ProviderMetadataInheritance::all(),
            )
            .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
        }
        let wasm_path = catalog.provider_path_by_id(&spec.provider.id);
        let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
        let caches = Caches::open(cache_dir.path()).map_err(|error| BuildError::CacheDir {
            path: cache_dir.path().to_path_buf(),
            source: std::io::Error::other(error.to_string()),
        })?;
        let credential_service =
            omnifs_engine::test_support::auth::credential_service_for_file(&paths.credentials_file);
        let context = HostContext::new(
            cache_dir.path(),
            &paths.config_dir,
            providers_dir.path(),
            &paths.credentials_file,
        );
        let runtime = match callouts {
            CalloutSetup::Captured => Engine::new_for_callout_tests(
                engine,
                &wasm_path,
                &spec,
                cloner,
                &context,
                &caches,
                &credential_service,
            ),
            CalloutSetup::Real => Engine::new(
                engine,
                &wasm_path,
                &spec,
                cloner,
                &context,
                &caches,
                &credential_service,
            ),
            CalloutSetup::Recorded(recorder) => Engine::new_with_callout_observer(
                engine,
                &wasm_path,
                &spec,
                cloner,
                &context,
                &caches,
                &credential_service,
                recorder,
            ),
        }?;

        Ok(RuntimeHarness {
            engine: engine.clone(),
            clone_dir,
            cache_dir,
            config_dir,
            providers_dir,
            runtime,
        })
    }
}

pub trait TestOpExt {
    fn expect_single_fetch(&self) -> &HttpRequest;
    fn expect_fetches(&self) -> Vec<&HttpRequest>;
    fn into_list_children(self) -> Result<ListChildrenResult, EngineError>;
    fn into_lookup_child(self) -> Result<LookupChildResult, EngineError>;
    fn into_read_file(self) -> Result<ReadFileResult, EngineError>;
}

impl TestOpExt for TestOp<'_> {
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

    fn into_list_children(self) -> Result<ListChildrenResult, EngineError> {
        match self.into_result()? {
            OpResult::ListChildren(result) => Ok(result),
            other => Err(EngineError::ProviderProtocol(format!(
                "expected list-children result, got {other:?}"
            ))),
        }
    }

    fn into_lookup_child(self) -> Result<LookupChildResult, EngineError> {
        match self.into_result()? {
            OpResult::LookupChild(result) => Ok(result),
            other => Err(EngineError::ProviderProtocol(format!(
                "expected lookup-child result, got {other:?}"
            ))),
        }
    }

    fn into_read_file(self) -> Result<ReadFileResult, EngineError> {
        match self.into_result()? {
            OpResult::ReadFile(ReadFileOutcome::Found(result)) => Ok(result),
            other => Err(EngineError::ProviderProtocol(format!(
                "expected found read-file result, got {other:?}"
            ))),
        }
    }
}

/// Stable on-disk wasm artifact cache shared across test processes. nextest
/// runs a process per test, so without a fixed directory every process would
/// recompile providers from scratch; a workspace-local dir keeps them warm.
fn itest_wasm_cache_dir() -> PathBuf {
    workspace_root().join("target").join("wasm-cache")
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

/// The served bytes of a completed read, whether the terminal serves the
/// canonical store (identity representations) or inline bytes (projections).
pub fn read_bytes(op: &TestOp<'_>) -> Vec<u8> {
    match op.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => match &file.bytes {
            ByteSource::Canonical => op.effects().unwrap().canonical[0].bytes.clone(),
            ByteSource::Inline(bytes) => bytes.clone(),
            other => panic!("expected canonical or inline read bytes, got {other:?}"),
        },
        other => panic!("expected a found read, got {other:?}"),
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
/// than require a manual `just providers build`, refresh the components on
/// demand.
///
/// This runs at test *runtime*, after cargo's build phase has released the
/// target-dir lock, so the build it triggers can write into the same
/// `target/wasm32-wasip2/release` that the test binary loads from (cache reused,
/// no second build tree) without deadlocking against the build that produced
/// this test binary.
///
/// It delegates to `just providers build` rather than invoking cargo directly:
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
            .args(["providers", "build"])
            .current_dir(workspace_root())
            .status()
            .expect("spawn `just providers build`");
        assert!(
            status.success(),
            "`just providers build` failed; run it directly to see the error",
        );
    });
}

pub fn make_engine() -> wasmtime::Engine {
    static ENGINE: OnceLock<wasmtime::Engine> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            omnifs_engine::test_support::component_engine(Some(&itest_wasm_cache_dir()), |_| {})
                .expect("build provider engine")
        })
        .clone()
}

/// The canonical test-provider mount config the bare `make_runtime` uses.
pub const TEST_PROVIDER_CONFIG: &str = r#"{"provider":"test_provider.wasm","mount":"test","capabilities":{"domains":["httpbin.org"]}}"#;

pub fn make_runtime(engine: &wasmtime::Engine) -> RuntimeHarness {
    RuntimeHarness::builder(TEST_PROVIDER_CONFIG)
        .engine(engine)
        .build()
        .unwrap()
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
        .add_artifact(artifact)
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

fn parse_path(path: &str) -> Path {
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
