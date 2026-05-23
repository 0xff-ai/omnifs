use omnifs_host::config::InstanceConfig;
use omnifs_host::omnifs::provider::types::{ReadFileBytes, ReadFileResult};
use omnifs_host::runtime::cloner::GitCloner;
use omnifs_host::runtime::tools::archive::{ArchiveExtractorComponent, DEFAULT_LIMITS};
use omnifs_host::runtime::{ProviderRuntime, RuntimeDirs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use tempfile::TempDir;

#[allow(dead_code)]
pub fn make_extractor() -> Arc<ArchiveExtractorComponent> {
    Arc::new(ArchiveExtractorComponent::new(DEFAULT_LIMITS).expect("build extractor"))
}

/// Borrow the inline payload of a `ReadFileResult`, panicking if the
/// terminal returned a blob-backed file. Tests that intentionally
/// exercise the blob path must match on the variant directly.
#[allow(dead_code)]
pub fn expect_inline(result: &ReadFileResult) -> &[u8] {
    match &result.bytes {
        ReadFileBytes::Inline(bytes) => bytes,
        ReadFileBytes::Blob(_) => panic!("expected inline file content, got blob-backed"),
    }
}

#[allow(dead_code)]
pub fn inline_content(result: &ReadFileResult) -> &[u8] {
    expect_inline(result)
}

#[allow(dead_code)]
pub fn into_inline(result: ReadFileResult) -> Vec<u8> {
    match result.bytes {
        ReadFileBytes::Inline(bytes) => bytes,
        ReadFileBytes::Blob(_) => panic!("expected inline file content, got blob-backed"),
    }
}

#[allow(dead_code)]
pub struct RuntimeHarness {
    pub _engine: wasmtime::Engine,
    pub clone_dir: TempDir,
    pub _cache_dir: TempDir,
    pub _config_dir: TempDir,
    pub runtime: ProviderRuntime,
}

#[allow(dead_code)]
pub fn provider_wasm_path(provider_name: &str) -> PathBuf {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let path = workspace_root
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join(provider_name);
    assert!(
        path.exists(),
        "{provider_name} not found at {path}. Run `just build-providers` first.",
        path = path.display()
    );
    path
}

#[allow(dead_code)]
pub fn make_engine() -> wasmtime::Engine {
    let mut wasm_config = wasmtime::Config::new();
    wasm_config.wasm_component_model(true);
    wasmtime::Engine::new(&wasm_config).unwrap()
}

#[allow(dead_code)]
pub fn make_runtime(engine: &wasmtime::Engine) -> RuntimeHarness {
    let config = InstanceConfig::parse(
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
    .unwrap();

    let clone_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
    let wasm_path = provider_wasm_path(&config.provider);
    let effective_config = config.into_effective("test_provider", None).unwrap();
    let runtime = ProviderRuntime::new(
        engine,
        &wasm_path,
        &effective_config,
        cloner,
        RuntimeDirs::new(
            cache_dir.path(),
            config_dir.path(),
            config_dir.path(),
            config_dir.path(),
        ),
        make_extractor(),
    )
    .unwrap();

    RuntimeHarness {
        _engine: engine.clone(),
        clone_dir,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
        runtime,
    }
}

#[allow(dead_code)]
pub fn try_make_runtime_from_config(
    config_json: &str,
) -> Result<RuntimeHarness, omnifs_host::runtime::RuntimeBuildError> {
    let config = InstanceConfig::parse(config_json).unwrap();
    let engine = make_engine();
    let clone_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
    let wasm_path = provider_wasm_path(&config.provider);
    let effective_config = config.into_effective("test_provider", None).unwrap();
    let runtime = ProviderRuntime::new(
        &engine,
        &wasm_path,
        &effective_config,
        cloner,
        RuntimeDirs::new(
            cache_dir.path(),
            config_dir.path(),
            config_dir.path(),
            config_dir.path(),
        ),
        make_extractor(),
    )?;

    Ok(RuntimeHarness {
        _engine: engine,
        clone_dir,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
        runtime,
    })
}

#[allow(dead_code)]
pub fn make_runtime_from_config(config_json: &str) -> RuntimeHarness {
    try_make_runtime_from_config(config_json).unwrap()
}

#[allow(dead_code)]
pub fn make_initialized_runtime(config_json: &str) -> RuntimeHarness {
    make_runtime_from_config(config_json)
}

/// Initialises a git repo in `dir` with a README and a src/main.rs, then
/// commits them. Used by tests that need a real local repo for the git
/// executor or for seeding the clone cache. The README content is caller-
/// supplied so tests can assert on it.
#[allow(dead_code)]
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
