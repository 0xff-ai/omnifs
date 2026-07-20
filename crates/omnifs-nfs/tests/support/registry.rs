use omnifs_engine::MountTable;
use omnifs_workspace::mounts::Registry;
use std::path::Path;
use std::time::Duration;

pub fn load_registry_from_mount_dir(
    cache_dir: &Path,
    _config_dir: &Path,
    providers_dir: &Path,
    credentials_file: &Path,
    clone_dir: &Path,
    mounts_dir: &Path,
    handle: &tokio::runtime::Handle,
) -> MountTable {
    let host = omnifs_engine::test_support::open_test_host(
        cache_dir,
        providers_dir,
        credentials_file,
        clone_dir,
    )
    .expect("open test host");
    let desired = Registry::load(mounts_dir).expect("load mount snapshot");
    let registry = MountTable::load_online(&host, &desired, handle)
        .unwrap_or_else(|error| panic!("load mount snapshot: {error}"));

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
