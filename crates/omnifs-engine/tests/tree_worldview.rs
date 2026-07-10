//! Kernel-free Worldview serving-scope tests over a real `MountRuntimes`
//! registry. These prove the scope at the shared tree boundary before FUSE or
//! NFS protocol translation can observe a path.

#![cfg(not(target_os = "wasi"))]

use std::sync::Arc;

use omnifs_core::path::Path;
use omnifs_engine::view::{EntryMeta, FileAttrsCache, FileSize, ReadMode, Stability};
use omnifs_engine::{
    GitCloner, HostContext, ListOutcome, MountRuntimes, Node, NodeBody, ReadResult, RequestCtx,
    ServingContext, Tree, TreeErrorKind,
};
use omnifs_itest::provider_wasm_path;
use omnifs_workspace::ids::ProviderRef;
use omnifs_workspace::mounts::Spec;
use omnifs_workspace::provider::{Artifact, ProviderStore};
use omnifs_workspace::worldviews::Worldview;
use tempfile::TempDir;
use tokio::runtime::Handle;

struct WorldviewTree {
    tree: Tree,
    _registry: Arc<MountRuntimes>,
    _clone_dir: TempDir,
    _cache_dir: TempDir,
    _config_dir: TempDir,
    _providers_dir: TempDir,
}

fn worldview_tree() -> WorldviewTree {
    let clone_dir = tempfile::tempdir().expect("clone dir");
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let config_dir = tempfile::tempdir().expect("config dir");
    let providers_dir = tempfile::tempdir().expect("providers dir");
    let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());
    let provider = install_test_provider(providers_dir.path());
    let registry = Arc::new(
        MountRuntimes::new(
            HostContext::new(
                cache_dir.path(),
                config_dir.path(),
                providers_dir.path(),
                &paths.credentials_file,
            ),
            Arc::new(GitCloner::new(clone_dir.path().to_path_buf())),
        )
        .expect("mount runtimes"),
    );

    for mount in ["scoped", "hidden"] {
        registry
            .add_mount(&spec(&provider, mount), &Handle::current())
            .unwrap_or_else(|error| panic!("add mount {mount}: {error}"));
    }

    let worldview = Worldview::parse(
        r#"{
            "name": "dev",
            "mounts": [
                { "mount": "scoped", "subtree": "/hello/bundle", "read_only": true }
            ]
        }"#,
    )
    .expect("worldview parses");
    let tree = Tree::new(ServingContext::from_worldview(
        Arc::clone(&registry),
        &worldview,
    ));

    WorldviewTree {
        tree,
        _registry: registry,
        _clone_dir: clone_dir,
        _cache_dir: cache_dir,
        _config_dir: config_dir,
        _providers_dir: providers_dir,
    }
}

fn install_test_provider(providers_dir: &std::path::Path) -> ProviderRef {
    let provider_file = "test_provider.wasm";
    let path = provider_wasm_path(provider_file);
    let bytes =
        std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let artifact = Artifact::from_bytes(provider_file, bytes)
        .unwrap_or_else(|error| panic!("{provider_file}: {error}"));
    let reference = artifact.reference();
    ProviderStore::new(providers_dir)
        .add_artifact(artifact)
        .expect("install test provider");
    reference
}

fn spec(provider: &ProviderRef, mount: &str) -> Spec {
    let mut value = serde_json::json!({
        "provider": provider,
        "mount": mount,
        "capabilities": { "domains": ["httpbin.org"] }
    });
    value["provider"] = serde_json::to_value(provider).expect("provider ref json");
    serde_json::from_value(value).expect("test provider spec")
}

fn path(value: &str) -> Path {
    Path::parse(value).unwrap_or_else(|error| panic!("test path {value}: {error}"))
}

fn provider_dir(mount: &str, value: &str) -> Node {
    Node::provider_dir(mount, path(value))
}

fn provider_file(mount: &str, value: &str) -> Node {
    Node::provider_file(mount, path(value), None)
}

fn ranged_file(mount: &str, value: &str) -> Node {
    Node::new(
        mount.to_string(),
        path(value),
        EntryMeta::file(
            FileAttrsCache::deferred(
                FileSize::Unknown,
                ReadMode::Ranged,
                Stability::Dynamic,
                None,
            )
            .expect("valid ranged attrs"),
        ),
        NodeBody::Provider,
    )
}

fn listing_names(outcome: ListOutcome) -> Vec<String> {
    let ListOutcome::Listing(listing) = outcome else {
        panic!("expected listing");
    };
    listing
        .entries
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn worldview_filters_mount_enumeration() {
    let t = worldview_tree();
    let ctx = RequestCtx::default();

    let root = t
        .tree
        .resolve(&path("/"), &ctx)
        .await
        .expect("resolve root");
    let names = listing_names(t.tree.list(&root, None, &ctx).await.expect("list root"));

    assert_eq!(names, ["scoped"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn worldview_synthesizes_ancestor_chain_to_subtree() {
    let t = worldview_tree();
    let ctx = RequestCtx::default();

    let mount_root = t
        .tree
        .resolve(&path("/scoped"), &ctx)
        .await
        .expect("resolve scoped mount root");
    let root_names = listing_names(
        t.tree
            .list(&mount_root, None, &ctx)
            .await
            .expect("list scoped mount root"),
    );
    assert_eq!(root_names, ["hello"]);

    let hello = t
        .tree
        .resolve(&path("/scoped/hello"), &ctx)
        .await
        .expect("resolve synthetic hello ancestor");
    let hello_names = listing_names(
        t.tree
            .list(&hello, None, &ctx)
            .await
            .expect("list synthetic hello ancestor"),
    );
    assert_eq!(hello_names, ["bundle"]);

    let bundle = t
        .tree
        .resolve(&path("/scoped/hello/bundle"), &ctx)
        .await
        .expect("resolve scoped subtree root");
    let bundle_names = listing_names(
        t.tree
            .list(&bundle, None, &ctx)
            .await
            .expect("list scoped subtree root"),
    );
    assert!(bundle_names.contains(&"title".to_string()));
    assert!(bundle_names.contains(&"body".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn worldview_resolve_not_found_outside_scope_or_hidden_mount() {
    let t = worldview_tree();
    let ctx = RequestCtx::default();

    let outside = t
        .tree
        .resolve(&path("/scoped/hello/message"), &ctx)
        .await
        .expect_err("valid provider path outside subtree must be hidden");
    assert_eq!(outside.kind, TreeErrorKind::NotFound);

    let hidden_mount = t
        .tree
        .resolve(&path("/hidden/hello/bundle"), &ctx)
        .await
        .expect_err("mount omitted from worldview must not exist");
    assert_eq!(hidden_mount.kind, TreeErrorKind::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn worldview_list_not_found_outside_scope() {
    let t = worldview_tree();
    let ctx = RequestCtx::default();

    let error = t
        .tree
        .list(&provider_dir("scoped", "/items"), None, &ctx)
        .await
        .expect_err("direct list outside scope must not dispatch");

    assert_eq!(error.kind, TreeErrorKind::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn worldview_read_not_found_outside_scope() {
    let t = worldview_tree();
    let ctx = RequestCtx::default();

    let error = t
        .tree
        .read(&provider_file("scoped", "/hello/message"), &ctx)
        .await
        .expect_err("direct read outside scope must not dispatch");

    assert_eq!(error.kind, TreeErrorKind::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn worldview_open_not_found_outside_scope() {
    let t = worldview_tree();
    let ctx = RequestCtx::default();

    let Err(error) = t
        .tree
        .open(&ranged_file("scoped", "/hello/ranged"), &ctx)
        .await
    else {
        panic!("direct open outside scope must not dispatch");
    };

    assert_eq!(error.kind, TreeErrorKind::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn worldview_serves_reads_inside_scope() {
    let t = worldview_tree();
    let ctx = RequestCtx::default();

    let node = t
        .tree
        .resolve(&path("/scoped/hello/bundle/title"), &ctx)
        .await
        .expect("resolve file inside scope");
    let ReadResult::Bytes { data, .. } = t
        .tree
        .read(&node, &ctx)
        .await
        .expect("read file inside scope")
    else {
        panic!("expected provider bytes");
    };

    assert_eq!(data, b"title");
}
