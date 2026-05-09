use omnifs_host::config::InstanceConfig;
use omnifs_host::omnifs::provider::types::{FileContentResult, InlineFileContent};
use omnifs_host::runtime::CalloutRuntime;
use omnifs_host::runtime::cloner::GitCloner;
use omnifs_host::runtime::tools::archive::ArchiveExtractorComponent;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use tempfile::TempDir;

#[allow(dead_code)]
pub fn make_extractor() -> Arc<ArchiveExtractorComponent> {
    Arc::new(ArchiveExtractorComponent::new().expect("build extractor"))
}

/// Borrow the inline payload of a `FileContentResult`, panicking if the
/// terminal returned a blob-backed file. Tests that intentionally
/// exercise the blob path must match on the variant directly.
#[allow(dead_code)]
pub fn expect_inline(result: &FileContentResult) -> &InlineFileContent {
    match result {
        FileContentResult::Inline(inline) => inline,
        FileContentResult::Blob(_) => panic!("expected inline file content, got blob-backed"),
    }
}

#[allow(dead_code)]
pub fn inline_content(result: &FileContentResult) -> &[u8] {
    expect_inline(result).content.as_slice()
}

#[allow(dead_code)]
pub fn into_inline(result: FileContentResult) -> InlineFileContent {
    match result {
        FileContentResult::Inline(inline) => inline,
        FileContentResult::Blob(_) => panic!("expected inline file content, got blob-backed"),
    }
}

#[allow(dead_code)]
pub struct RuntimeHarness {
    pub _engine: wasmtime::Engine,
    pub clone_dir: TempDir,
    pub _cache_dir: TempDir,
    pub runtime: CalloutRuntime,
}

#[allow(dead_code)]
pub fn provider_wasm_path(plugin_name: &str) -> PathBuf {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let path = workspace_root
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join(plugin_name);
    assert!(
        path.exists(),
        "{plugin_name} not found at {path}. Run `just build-providers` first.",
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
            "plugin": "test_provider.wasm",
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
    let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
    let runtime = CalloutRuntime::new(
        engine,
        &provider_wasm_path(&config.plugin),
        &config,
        cloner,
        cache_dir.path(),
        "test-mount",
        make_extractor(),
    )
    .unwrap();

    RuntimeHarness {
        _engine: engine.clone(),
        clone_dir,
        _cache_dir: cache_dir,
        runtime,
    }
}

#[allow(dead_code)]
pub fn make_runtime_from_config(config_json: &str) -> RuntimeHarness {
    let config = InstanceConfig::parse(config_json).unwrap();
    let engine = make_engine();
    let clone_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
    let runtime = CalloutRuntime::new(
        &engine,
        &provider_wasm_path(&config.plugin),
        &config,
        cloner,
        cache_dir.path(),
        &config.mount,
        make_extractor(),
    )
    .unwrap();

    RuntimeHarness {
        _engine: engine,
        clone_dir,
        _cache_dir: cache_dir,
        runtime,
    }
}

#[allow(dead_code)]
pub fn make_initialized_runtime(config_json: &str) -> RuntimeHarness {
    let harness = make_runtime_from_config(config_json);
    harness.runtime.initialize().unwrap();
    harness
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
