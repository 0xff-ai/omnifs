use omnifs_engine::MountTable;
use omnifs_engine::{MountBuildInput, MountBuildState, ProviderBuildInput, RuntimeMountConfig};
use omnifs_provider::Artifact;
use std::path::Path;
use std::time::Duration;

pub fn load_registry_from_mount_dir(
    cache_dir: &Path,
    clone_dir: &Path,
    mounts_dir: &Path,
    handle: &tokio::runtime::Handle,
) -> MountTable {
    let host =
        omnifs_engine::test_support::open_test_host(cache_dir, clone_dir).expect("open test host");
    let spec_path = std::fs::read_dir(mounts_dir)
        .expect("read mount specs")
        .map(|entry| entry.expect("mount spec entry").path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .expect("test mount spec");
    let canonical = std::fs::read(&spec_path).expect("read mount spec");
    let spec: serde_json::Value = serde_json::from_slice(&canonical).expect("parse mount spec");
    let mount = spec
        .get("mount")
        .and_then(serde_json::Value::as_str)
        .or_else(|| spec_path.file_stem().and_then(|stem| stem.to_str()))
        .expect("mount name");
    let config = spec
        .get("config")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let provider_path = super::provider_wasm_path("test_provider.wasm");
    let provider_bytes = std::fs::read(provider_path).expect("read test provider");
    let (artifact, manifest) =
        Artifact::from_bytes_with_manifest("test_provider.wasm", provider_bytes.clone())
            .expect("parse test provider artifact");
    let registry = MountTable::prepare_durable(
        &host,
        vec![MountBuildInput {
            config: RuntimeMountConfig {
                name: omnifs_core::ResourceName::new(mount).expect("mount name"),
                provider: artifact.reference(),
                config,
                max_fetch_blob_bytes: None,
            },
            canonical: std::sync::Arc::from(canonical.into_boxed_slice()),
            provider: Some(ProviderBuildInput {
                bytes: std::sync::Arc::from(provider_bytes.into_boxed_slice()),
                manifest,
            }),
            state: MountBuildState::Active {
                auth: None,
                credential_generation: None,
            },
        }],
    )
    .unwrap_or_else(|error| panic!("load mount snapshot: {error}"));
    registry.activate_timers(handle);

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
