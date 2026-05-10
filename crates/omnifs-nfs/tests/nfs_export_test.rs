mod support;

use omnifs_host::registry::ProviderRegistry;
use omnifs_host::runtime::cloner::GitCloner;
use omnifs_nfs::{NFS4ERR_NOTDIR, NfsNodeKind, OmnifsExport, ReadOnlyExport};
use std::sync::Arc;
use support::provider_wasm_path;

#[test]
fn omnifs_export_lists_and_reads_through_runtime() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let clone_dir = tempfile::tempdir().expect("clone dir");
    let providers_dir = config_dir.path().join("providers");
    let plugin_dir = config_dir.path().join("plugins");
    std::fs::create_dir_all(&providers_dir).expect("providers dir");
    std::fs::create_dir_all(&plugin_dir).expect("plugin dir");
    std::fs::copy(
        provider_wasm_path("test_provider.wasm"),
        plugin_dir.join("test_provider.wasm"),
    )
    .expect("copy test provider");
    std::fs::write(
        providers_dir.join("test.json"),
        r#"{
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }"#,
    )
    .expect("provider config");

    let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
    let registry =
        ProviderRegistry::load(config_dir.path(), &plugin_dir, &cloner, cache_dir.path())
            .expect("registry should load");
    for mount in registry.mounts() {
        registry
            .get(&mount)
            .expect("runtime")
            .initialize()
            .expect("provider init");
    }

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let export = OmnifsExport::new(runtime.handle().clone(), Arc::new(registry));
    let test_root = export
        .lookup(export.root(), "test")
        .expect("top-level mount lookup");
    let hello = export.lookup(test_root, "hello").expect("hello lookup");
    let hello_attr = export.attr(hello).expect("hello attr");
    assert_eq!(hello_attr.kind, NfsNodeKind::Directory);

    let listing = export.readdir(hello).expect("hello listing");
    let names = listing
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"message"));
    assert!(names.contains(&"bundle"));

    let message = export.lookup(hello, "message").expect("message lookup");
    let message_attr = export.attr(message).expect("message attr");
    assert_eq!(message_attr.kind, NfsNodeKind::File);
    assert_eq!(
        export.read(message).expect("message read"),
        b"Hello, world!".to_vec()
    );
    assert_eq!(
        export.attr(message).expect("message attr after read").size,
        13
    );

    let _listing_after_read = export.readdir(hello).expect("hello relisting");
    assert_eq!(
        export
            .attr(message)
            .expect("message attr after relisting")
            .size,
        13
    );

    let bundle = export.lookup(hello, "bundle").expect("bundle lookup");
    for entry in export.readdir(bundle).expect("bundle listing") {
        if entry.attr.kind == NfsNodeKind::File {
            assert!(matches!(export.readdir(entry.id), Err(NFS4ERR_NOTDIR)));
        }
    }
}

#[test]
fn omnifs_export_accepts_named_export_without_extra_directory() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let clone_dir = tempfile::tempdir().expect("clone dir");
    let providers_dir = config_dir.path().join("providers");
    let plugin_dir = config_dir.path().join("plugins");
    std::fs::create_dir_all(&providers_dir).expect("providers dir");
    std::fs::create_dir_all(&plugin_dir).expect("plugin dir");
    std::fs::copy(
        provider_wasm_path("test_provider.wasm"),
        plugin_dir.join("test_provider.wasm"),
    )
    .expect("copy test provider");
    std::fs::write(
        providers_dir.join("test.json"),
        r#"{
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }"#,
    )
    .expect("provider config");

    let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
    let registry =
        ProviderRegistry::load(config_dir.path(), &plugin_dir, &cloner, cache_dir.path())
            .expect("registry should load");
    for mount in registry.mounts() {
        registry
            .get(&mount)
            .expect("runtime")
            .initialize()
            .expect("provider init");
    }

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let export = OmnifsExport::new(runtime.handle().clone(), Arc::new(registry));
    let named_export = export
        .lookup(export.root(), "omnifs")
        .expect("named export lookup");
    assert_ne!(named_export, export.root());

    let listing = export.readdir(named_export).expect("named export listing");
    let names = listing
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["test"]);
}

#[test]
fn omnifs_export_preserves_dynamic_prefix_lookup_after_implicit_dir_lookup() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let clone_dir = tempfile::tempdir().expect("clone dir");
    let providers_dir = config_dir.path().join("providers");
    let plugin_dir = config_dir.path().join("plugins");
    std::fs::create_dir_all(&providers_dir).expect("providers dir");
    std::fs::create_dir_all(&plugin_dir).expect("plugin dir");
    std::fs::copy(
        provider_wasm_path("test_provider.wasm"),
        plugin_dir.join("test_provider.wasm"),
    )
    .expect("copy test provider");
    std::fs::write(
        providers_dir.join("test.json"),
        r#"{
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }"#,
    )
    .expect("provider config");

    let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
    let registry =
        ProviderRegistry::load(config_dir.path(), &plugin_dir, &cloner, cache_dir.path())
            .expect("registry should load");
    for mount in registry.mounts() {
        registry
            .get(&mount)
            .expect("runtime")
            .initialize()
            .expect("provider init");
    }

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let export = OmnifsExport::new(runtime.handle().clone(), Arc::new(registry));
    let test_root = export
        .lookup(export.root(), "test")
        .expect("top-level mount lookup");
    let dynamic = export
        .lookup(test_root, "dynamic")
        .expect("implicit dynamic prefix lookup");
    let captured = export
        .lookup(dynamic, "alpha")
        .expect("captured dynamic child lookup");
    assert_eq!(
        export.attr(captured).expect("captured attr").kind,
        NfsNodeKind::Directory
    );
    let value = export
        .lookup(captured, "value")
        .expect("captured value lookup");
    assert_eq!(
        export.read(value).expect("captured value read"),
        b"alpha\n".to_vec()
    );
}
