use omnifs_engine::{MountRuntimes, TreeNamespace};
use omnifs_nfs::Export;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::runtime::Runtime;

mod registry;
use registry::load_registry_from_mount_dir;

// This module is compiled into two integration-test binaries. The socket test
// only reads `export`; the protocol export tests also drive the retained
// runtime, registry, and namespace directly.
#[allow(dead_code)]
pub struct TestExport {
    pub export: Arc<Export>,
    pub runtime: Runtime,
    pub registry: Arc<MountRuntimes>,
    pub namespace: Arc<TreeNamespace>,
    _config_dir: TempDir,
    _cache_dir: TempDir,
    _clone_dir: TempDir,
}

pub fn test_export() -> TestExport {
    test_export_with_mount("test")
}

pub fn test_export_with_mount(mount: &str) -> TestExport {
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
    let namespace = TreeNamespace::new(Arc::clone(&registry), runtime.handle().clone());
    let export = Arc::new(Export::new(
        runtime.handle().clone(),
        Arc::clone(&namespace) as Arc<dyn omnifs_engine::Namespace>,
    ));
    TestExport {
        export,
        runtime,
        registry,
        namespace,
        _config_dir: config_dir,
        _cache_dir: cache_dir,
        _clone_dir: clone_dir,
    }
}

/// The pinned reference for the test provider, derived from its built bytes.
pub fn test_provider_reference() -> omnifs_workspace::ids::ProviderRef {
    use omnifs_workspace::provider::Artifact;
    Artifact::from_file(provider_wasm_path("test_provider.wasm"))
        .expect("parse test provider")
        .reference()
}

/// Install the test provider into the content-addressed store at `providers_dir`.
fn install_test_provider(providers_dir: &Path) {
    use omnifs_workspace::provider::{Artifact, ProviderStore};
    let artifact =
        Artifact::from_file(provider_wasm_path("test_provider.wasm")).expect("parse test provider");
    let store = ProviderStore::new(providers_dir);
    store.retain(&artifact).expect("retain test provider");
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
        "{plugin_name} not found at {path}. Run `just build providers` first.",
        path = path.display()
    );
    path
}
