use omnifs_host::registry::ProviderRegistry;
use omnifs_nfs::Export;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::runtime::Runtime;

mod registry;
use registry::load_registry_from_mount_dir;

pub struct TestExport {
    pub export: Arc<Export>,
    #[allow(dead_code)]
    pub runtime: Runtime,
    #[allow(dead_code)]
    pub registry: Arc<ProviderRegistry>,
    _config_dir: TempDir,
    _cache_dir: TempDir,
    _clone_dir: TempDir,
}

pub fn test_export() -> TestExport {
    test_export_with_mount("test")
}

pub fn test_export_with_mount(mount: &str) -> TestExport {
    test_export_with_mount_options(mount, false)
}

#[allow(dead_code)]
pub fn root_mounted_test_export() -> TestExport {
    test_export_with_mount_options("test", true)
}

fn test_export_with_mount_options(mount: &str, root_mount: bool) -> TestExport {
    let config_dir = tempfile::tempdir().expect("config dir");
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let clone_dir = tempfile::tempdir().expect("clone dir");
    let mounts_dir = config_dir.path().join("mounts");
    let providers_dir = config_dir.path().join("providers");
    std::fs::create_dir_all(&mounts_dir).expect("mounts dir");
    std::fs::create_dir_all(&providers_dir).expect("providers dir");
    std::fs::copy(
        provider_wasm_path("test_provider.wasm"),
        providers_dir.join("test_provider.wasm"),
    )
    .expect("copy test provider");
    std::fs::copy(
        provider_wasm_path("omnifs_tool_archive.wasm"),
        providers_dir.join("omnifs_tool_archive.wasm"),
    )
    .expect("copy archive tool");
    let provider_config = format!(
        r#"{{
            "provider": "test_provider.wasm",
            "mount": {mount:?},
            "root_mount": {root_mount},
            "capabilities": {{
                "domains": ["httpbin.org"]
            }}
        }}"#,
    );
    std::fs::write(mounts_dir.join(format!("{mount}.json")), provider_config)
        .expect("mount config");

    let runtime = Runtime::new().expect("tokio runtime");
    let registry = load_registry_from_mount_dir(
        cache_dir.path(),
        config_dir.path(),
        &providers_dir,
        &config_dir.path().join("credentials.json"),
        clone_dir.path(),
        &mounts_dir,
        runtime.handle(),
    );
    let registry = Arc::new(registry);
    let export = Arc::new(Export::new(runtime.handle().clone(), Arc::clone(&registry)));
    TestExport {
        export,
        runtime,
        registry,
        _config_dir: config_dir,
        _cache_dir: cache_dir,
        _clone_dir: clone_dir,
    }
}

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
        "{plugin_name} not found at {path}. Run `just providers-build` first.",
        path = path.display()
    );
    path
}
