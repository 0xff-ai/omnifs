use omnifs_engine::{MountRuntimes, TreeNamespace};
use omnifs_itest::live::{install_test_provider, test_provider_reference};
use omnifs_nfs::Export;
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
    #[allow(dead_code)]
    pub namespace: Arc<TreeNamespace>,
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
    let _ = install_test_provider(&providers_dir);
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
