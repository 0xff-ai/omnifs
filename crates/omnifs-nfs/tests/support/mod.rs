use omnifs_core::path::Path as NamespacePath;
use omnifs_engine::{
    Attrs, DirCursor, DirPage, EventStream, LookupAnswer, MountTable, Namespace, NsError, NsEvent,
    ReadAnswer, TreeNamespace,
};
use omnifs_nfs::Export;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::runtime::Runtime;
use tokio::sync::broadcast;

mod registry;
use registry::load_registry_from_mount_dir;

// This module is compiled into two integration-test binaries. The socket test
// only reads `export`; the protocol export tests also drive the retained
// runtime, registry, and namespace directly.
#[allow(dead_code)]
pub struct TestExport {
    pub export: Arc<Export>,
    pub runtime: Runtime,
    pub registry: Arc<MountTable>,
    pub namespace: Arc<TreeNamespace>,
    pub events: broadcast::Sender<NsEvent>,
    _config_dir: TempDir,
    _cache_dir: TempDir,
    _clone_dir: TempDir,
}

struct EventNamespace {
    inner: Arc<TreeNamespace>,
    events: broadcast::Sender<NsEvent>,
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl Namespace for EventNamespace {
    fn lookup<'a>(
        &'a self,
        parent: NamespacePath,
        name: &'a str,
    ) -> BoxFuture<'a, Result<LookupAnswer, NsError>> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_owned();
        Box::pin(async move { inner.lookup(parent, &name).await })
    }

    fn getattr(&self, path: NamespacePath) -> BoxFuture<'_, Result<Attrs, NsError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.getattr(path).await })
    }

    fn getattr_exact(&self, path: NamespacePath) -> BoxFuture<'_, Result<Attrs, NsError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.getattr_exact(path).await })
    }

    fn readdir(
        &self,
        path: NamespacePath,
        cursor: DirCursor,
        budget: usize,
    ) -> BoxFuture<'_, Result<DirPage, NsError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.readdir(path, cursor, budget).await })
    }

    fn read(
        &self,
        path: NamespacePath,
        offset: u64,
        len: u32,
    ) -> BoxFuture<'_, Result<ReadAnswer, NsError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.read(path, offset, len).await })
    }

    fn readlink(&self, path: NamespacePath) -> BoxFuture<'_, Result<std::path::PathBuf, NsError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.readlink(path).await })
    }

    fn subscribe(&self) -> EventStream {
        EventStream::from_broadcast(self.events.subscribe())
    }
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
            "mount": {mount:?}
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
    let namespace = TreeNamespace::online(Arc::clone(&registry), runtime.handle().clone());
    let (events, _) = broadcast::channel(64);
    let mut inner_events = namespace.subscribe();
    let forwarded_events = events.clone();
    runtime.spawn(async move {
        while let Some(event) = inner_events.recv().await {
            let _ = forwarded_events.send(event);
        }
    });
    let export_namespace = Arc::new(EventNamespace {
        inner: Arc::clone(&namespace),
        events: events.clone(),
    });
    let export = Arc::new(Export::new(runtime.handle().clone(), export_namespace));
    TestExport {
        export,
        runtime,
        registry,
        namespace,
        events,
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
