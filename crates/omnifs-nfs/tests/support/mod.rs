use omnifs_engine::MountRuntimes;
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
    pub registry: Arc<MountRuntimes>,
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
    install_test_provider(&providers_dir);
    let reference = serde_json::to_string(&test_provider_reference()).expect("provider ref json");
    let provider_config = format!(
        r#"{{
            "provider": {reference},
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

/// The pinned reference for the test provider, derived from its built bytes.
pub fn test_provider_reference() -> omnifs_workspace::ids::ProviderRef {
    use omnifs_workspace::ids::{ProviderId, ProviderMeta, ProviderName, ProviderRef};
    let bytes =
        std::fs::read(provider_wasm_path("test_provider.wasm")).expect("read test provider");
    ProviderRef {
        id: ProviderId::from_wasm_bytes(&bytes),
        meta: ProviderMeta {
            name: ProviderName::new("test-provider").unwrap(),
            version: None,
        },
    }
}

/// A mount `Spec` for the test provider, pinned to the provider store any
/// `TestExport` installs it into.
#[allow(dead_code)]
pub fn test_provider_spec(mount: &str) -> omnifs_workspace::mounts::Spec {
    let value = serde_json::json!({
        "provider": test_provider_reference(),
        "mount": mount,
    });
    serde_json::from_value(value).expect("build test spec")
}

/// Install the test provider into the content-addressed store at `providers_dir`.
fn install_test_provider(providers_dir: &Path) {
    use omnifs_workspace::provider::ProviderStore;
    let reference = test_provider_reference();
    let bytes =
        std::fs::read(provider_wasm_path("test_provider.wasm")).expect("read test provider");
    let store = ProviderStore::new(providers_dir);
    store
        .put_if_absent(&reference.id, &bytes)
        .expect("put test provider");
    store
        .install(reference.id, reference.meta, "test_provider.wasm".into())
        .expect("install test provider");
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
        "{plugin_name} not found at {path}. Run `just providers build` first.",
        path = path.display()
    );
    path
}
