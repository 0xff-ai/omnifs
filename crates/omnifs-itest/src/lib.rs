use omnifs_cache::{Caches, Record as CacheRecord, RecordKind};
use omnifs_core::path::{Path as OmnifsPath, Segment};
use omnifs_host::cloner::GitCloner;
use omnifs_host::mounts::Spec;
use omnifs_host::tools::archive::{ARCHIVE_TOOL_WASM, ArchiveExtractorComponent, DEFAULT_LIMITS};
use omnifs_host::{BuildError, Dirs, Error, Op, Runtime, TestOp};
use omnifs_wit::provider::types::{
    ByteSource, Callout, Effects, HttpRequest, ListChildrenResult, LookupChildResult, OpResult,
    ReadFileOutcome, ReadFileResult,
};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use tempfile::TempDir;

/// Runtime fixture for provider integration tests.
///
/// The harness owns the temporary directories that must outlive the mounted
/// provider runtime. Provider execution itself is always delegated to
/// `omnifs-host`: tests do not build linkers, stores, or provider bindings.
pub struct RuntimeHarness {
    pub engine: wasmtime::Engine,
    pub clone_dir: TempDir,
    pub cache_dir: TempDir,
    pub config_dir: TempDir,
    pub runtime: Runtime,
}

impl RuntimeHarness {
    pub fn new(spec: Spec) -> Result<Self, BuildError> {
        let engine = make_engine();
        Self::with_engine(spec, &engine)
    }

    pub fn with_engine(spec: Spec, engine: &wasmtime::Engine) -> Result<Self, BuildError> {
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
        let provider_dir = provider_artifact_dir();
        let catalog = omnifs_host::mounts::Catalog::new(config_dir.path(), &provider_dir);
        let resolved = catalog
            .resolve_spec(spec, false)
            .map_err(|error| BuildError::InvalidConfig(error.to_string()))?;
        let wasm_path = catalog.provider_path(&resolved);
        let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
        let caches = Caches::open(cache_dir.path()).map_err(|error| BuildError::CacheDir {
            path: cache_dir.path().to_path_buf(),
            source: std::io::Error::other(error.to_string()),
        })?;
        let runtime = Runtime::new(
            engine,
            &wasm_path,
            &resolved,
            cloner,
            Dirs::new(
                cache_dir.path(),
                config_dir.path(),
                config_dir.path(),
                &provider_dir,
            ),
            make_extractor(),
            &caches,
        )?;

        Ok(Self {
            engine: engine.clone(),
            clone_dir,
            cache_dir,
            config_dir,
            runtime,
        })
    }

    pub fn start_op(&self, op: Op) -> Result<TestOp<'_>, Error> {
        self.runtime.start_op(op)
    }

    pub fn lookup(&self, parent_path: &str, name: &str) -> Result<TestOp<'_>, Error> {
        self.start_op(Op::LookupChild {
            parent_path: parse_path(parent_path),
            name: Segment::try_from(name).expect("test lookup name must be a protocol segment"),
        })
    }

    pub fn list(&self, path: &str) -> Result<TestOp<'_>, Error> {
        self.list_with_cursor(path, None)
    }

    pub fn list_with_cursor(
        &self,
        path: &str,
        cursor: Option<omnifs_wit::provider::types::Cursor>,
    ) -> Result<TestOp<'_>, Error> {
        self.start_op(Op::ListChildren {
            path: parse_path(path),
            cached_validator: None,
            cursor,
        })
    }

    pub fn read(&self, path: &str) -> Result<TestOp<'_>, Error> {
        let path = parse_path(path);
        self.start_op(Op::ReadFile {
            content_type: path.content_type_mime(None).to_string(),
            path,
            cached_canonical: None,
        })
    }

    pub fn timer_tick(&self) -> Result<TestOp<'_>, Error> {
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
        self.runtime.cache_get(path, kind, aux)
    }

    pub fn cached_canonical_for(&self, path: &str) -> Option<(Vec<u8>, Vec<u8>, Option<String>)> {
        self.runtime.cached_canonical_for(path)
    }

    pub fn current_generation(&self) -> u64 {
        self.runtime.current_generation()
    }
}

pub trait TestOpExt {
    fn expect_single_fetch(&self) -> &HttpRequest;
    fn expect_fetches(&self) -> Vec<&HttpRequest>;
    fn into_list_children(self) -> Result<ListChildrenResult, Error>;
    fn into_lookup_child(self) -> Result<LookupChildResult, Error>;
    fn into_read_file(self) -> Result<ReadFileResult, Error>;
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

    fn into_list_children(self) -> Result<ListChildrenResult, Error> {
        match self.into_result()? {
            OpResult::ListChildren(result) => Ok(result),
            other => Err(Error::ProviderProtocol(format!(
                "expected list-children result, got {other:?}"
            ))),
        }
    }

    fn into_lookup_child(self) -> Result<LookupChildResult, Error> {
        match self.into_result()? {
            OpResult::LookupChild(result) => Ok(result),
            other => Err(Error::ProviderProtocol(format!(
                "expected lookup-child result, got {other:?}"
            ))),
        }
    }

    fn into_read_file(self) -> Result<ReadFileResult, Error> {
        match self.into_result()? {
            OpResult::ReadFile(ReadFileOutcome::Found(result)) => Ok(result),
            other => Err(Error::ProviderProtocol(format!(
                "expected found read-file result, got {other:?}"
            ))),
        }
    }
}

pub fn make_extractor() -> Arc<ArchiveExtractorComponent> {
    Arc::new(
        ArchiveExtractorComponent::from_path(provider_wasm_path(ARCHIVE_TOOL_WASM), DEFAULT_LIMITS)
            .expect("build extractor"),
    )
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

pub fn inline_content(result: &ReadFileResult) -> &[u8] {
    expect_inline(result)
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
    assert!(
        path.exists(),
        "{provider_name} not found at {path}. Run `just providers-build` first.",
        path = path.display()
    );
    path
}

pub fn make_engine() -> wasmtime::Engine {
    static ENGINE: OnceLock<wasmtime::Engine> = OnceLock::new();
    ENGINE
        .get_or_init(|| omnifs_host::component_engine(|_| {}).expect("build provider engine"))
        .clone()
}

pub fn make_runtime(engine: &wasmtime::Engine) -> RuntimeHarness {
    RuntimeHarness::with_engine(test_provider_spec(), engine).unwrap()
}

pub fn try_make_runtime_from_config(
    config_json: &str,
) -> Result<RuntimeHarness, omnifs_host::BuildError> {
    RuntimeHarness::new(Spec::parse(config_json).unwrap())
}

pub fn make_runtime_from_config(config_json: &str) -> RuntimeHarness {
    try_make_runtime_from_config(config_json).unwrap()
}

pub fn make_initialized_runtime(config_json: &str) -> RuntimeHarness {
    make_runtime_from_config(config_json)
}

pub fn project_paths(effects: &Effects) -> Vec<&str> {
    effects.fs.iter().map(|write| write.path.as_str()).collect()
}

pub fn test_provider_spec() -> Spec {
    Spec::parse(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    )
    .unwrap()
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn parse_path(path: &str) -> OmnifsPath {
    OmnifsPath::parse(path)
        .unwrap_or_else(|error| panic!("test path must be absolute: {path}: {error}"))
}

/// Initialises a git repo in `dir` with a README and a src/main.rs, then
/// commits them. Used by tests that need a real local repo for the git
/// executor or for seeding the clone cache. The README content is caller-
/// supplied so tests can assert on it.
pub fn create_test_repo(dir: &Path, readme_content: &str) {
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
