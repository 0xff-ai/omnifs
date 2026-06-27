use omnifs_host::HostContext;
use omnifs_host::cloner::GitCloner;
use omnifs_host::registry::ProviderRegistry;
use omnifs_mount::mounts::Spec;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub fn load_registry_from_mount_dir(
    cache_dir: &Path,
    config_dir: &Path,
    providers_dir: &Path,
    credentials_file: &Path,
    clone_dir: &Path,
    mounts_dir: &Path,
    handle: &tokio::runtime::Handle,
) -> ProviderRegistry {
    let cloner = Arc::new(GitCloner::new(clone_dir.to_path_buf()));
    let context = HostContext::new(cache_dir, config_dir, providers_dir, credentials_file);
    let registry =
        ProviderRegistry::new(context, Arc::clone(&cloner)).expect("registry should load");

    let mut mount_files = std::fs::read_dir(mounts_dir)
        .expect("mounts dir")
        .map(|entry| entry.expect("mount entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    mount_files.sort();
    for path in mount_files {
        let bytes = std::fs::read(&path).expect("read mount spec");
        let spec = serde_json::from_slice::<Spec>(&bytes).expect("parse mount spec");
        registry
            .add_mount(&spec, handle)
            .unwrap_or_else(|error| panic!("load mount {}: {error}", path.display()));
    }

    // The provider timer interval fires once immediately after spawn. Tests
    // that assert explicit invalidation behavior start from a quiet fixture.
    handle.block_on(async {
        tokio::time::sleep(Duration::from_millis(50)).await;
    });
    for (_mount, runtime) in registry.runtime_entries() {
        let _ = runtime.drain_invalidated_prefixes();
        let _ = runtime.drain_invalidated_paths();
    }

    registry
}
