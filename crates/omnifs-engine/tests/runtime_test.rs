use omnifs_core::path::Path;
use omnifs_engine::test_support::cache::publish_effects_for_test;
use omnifs_engine::{
    DirCursor, EntryKind, LookupAnswer, Namespace, NsError, ReadAnswer, TreeNamespace,
};
use omnifs_itest::{TEST_PROVIDER_CONFIG, make_initialized_runtime, make_runtime};

fn p(value: &str) -> Path {
    Path::parse(value).unwrap()
}

async fn resolve_namespace(ns: &TreeNamespace, path: &str) -> LookupAnswer {
    resolve_mount_namespace(ns, "test", path).await
}

async fn resolve_mount_namespace(ns: &TreeNamespace, mount: &str, path: &str) -> LookupAnswer {
    let mut answer = ns.lookup(Path::root(), mount).await.unwrap();
    for segment in p(path).segments() {
        answer = ns.lookup(answer.path, segment).await.unwrap();
    }
    answer
}

async fn list_mount_namespace(
    ns: &TreeNamespace,
    mount: &str,
    path: &str,
) -> Result<Vec<omnifs_engine::DirEntry>, NsError> {
    let node = resolve_mount_namespace(ns, mount, path).await;
    let mut cursor = DirCursor::start();
    let mut entries = Vec::new();
    loop {
        let page = ns.readdir(node.path.clone(), cursor, 0).await?;
        entries.extend(page.entries);
        match page.next {
            Some(next) => cursor = next,
            None => return Ok(entries),
        }
    }
}

async fn list_namespace(
    ns: &TreeNamespace,
    path: &str,
) -> Result<Vec<omnifs_engine::DirEntry>, NsError> {
    let node = resolve_namespace(ns, path).await;
    let mut cursor = DirCursor::start();
    let mut entries = Vec::new();
    loop {
        let page = ns.readdir(node.path.clone(), cursor, 0).await?;
        entries.extend(page.entries);
        match page.next {
            Some(next) => cursor = next,
            None => return Ok(entries),
        }
    }
}

async fn read_namespace(ns: &TreeNamespace, path: &str) -> Result<ReadAnswer, NsError> {
    let node = resolve_namespace(ns, path).await;
    ns.read(node.path.clone(), 0, u32::MAX).await
}

/// Every shipped provider must initialize (run `start()` + `Router::compile()`) cleanly.
/// Router compilation runs inside `initialize` and needs no credentials, so this is a
/// deterministic gate for route-overlap and registration errors that a
/// `cargo check` for `wasm32-wasip2` cannot catch (it type-checks but never compiles the route tree).
/// This guards against the class of bug where a migrated provider mounts an
/// object at the wrong template (e.g. an object at `/{a}/{b}` colliding with a
/// literal route), which otherwise only surfaces at live mount time.
#[tokio::test]
async fn all_providers_initialize_and_compile() {
    // Providers whose `start()` registers routes without touching a backing
    // resource. `db` is excluded: it opens its SQLite file at init, so a bare
    // harness (no fixture) fails with an environmental I/O error, not a route
    // compilation error; db's route compilation is exercised through its live
    // mount instead.
    let providers = [
        ("omnifs_provider_github.wasm", "github"),
        ("omnifs_provider_arxiv.wasm", "arxiv"),
        ("omnifs_provider_dns.wasm", "dns"),
        ("omnifs_provider_docker.wasm", "docker"),
        ("omnifs_provider_kubernetes.wasm", "k8s"),
        ("omnifs_provider_linear.wasm", "linear"),
        ("omnifs_provider_oura.wasm", "oura"),
        ("omnifs_provider_web.wasm", "web"),
    ];
    for (wasm, mount) in providers {
        let config = if wasm == "omnifs_provider_web.wasm" {
            format!(
                r#"{{
                    "provider":"{wasm}",
                    "mount":"{mount}",
                    "config": {{ "domains": ["example.com"] }}
                }}"#
            )
        } else {
            format!(r#"{{"provider":"{wasm}","mount":"{mount}"}}"#)
        };
        let result = omnifs_itest::try_make_runtime_from_config(&config);
        assert!(
            result.is_ok(),
            "provider {wasm} failed to initialize/compile: {:?}",
            result.err()
        );
    }
}

#[tokio::test]
async fn test_list_root() {
    let harness = make_runtime();
    let entries = list_namespace(&harness.namespace, "/").await.unwrap();
    {
        assert_eq!(entries.len(), 9);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(names.contains(&"README.md"));
        assert!(names.contains(&"items"));
        assert!(names.contains(&"hello"));
        assert!(names.contains(&"scoped"));
        assert!(names.contains(&"dynamic"));
        assert!(names.contains(&"slow"));
        assert!(names.contains(&".gitignore"));
        assert!(names.contains(&".ignore"));
        assert!(names.contains(&".rgignore"));
        assert!(
            entries
                .iter()
                .filter(|entry| !matches!(
                    entry.name.as_str(),
                    "README.md" | ".gitignore" | ".ignore" | ".rgignore"
                ))
                .all(|entry| entry.attrs.kind == EntryKind::Directory)
        );
    }
}

#[tokio::test]
async fn test_list_hello_dir() {
    let harness = make_runtime();
    let entries = list_namespace(&harness.namespace, "/hello").await.unwrap();
    {
        assert_eq!(entries.len(), 18);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"README.md"));
        assert!(names.contains(&"remote-a"));
        assert!(names.contains(&"remote-b"));
        assert!(names.contains(&"message"));
        assert!(names.contains(&"large-ranged"));
        assert!(names.contains(&"greeting"));
        assert!(names.contains(&"projected"));
        assert!(names.contains(&"lazy"));
        assert!(names.contains(&"fresh-full"));
        assert!(names.contains(&"ranged"));
        assert!(names.contains(&"unknown-ranged"));
        assert!(names.contains(&"large-ranged"));
        assert!(names.contains(&"volatile-tail"));
        assert!(names.contains(&"live-log"));
        assert!(names.contains(&"bundle"));
        assert!(names.contains(&"feed"));
        assert!(names.contains(&"snapshot"));
        assert!(names.contains(&"throttled"));
        assert!(names.contains(&"unbounded"));
    }
    let readme = resolve_namespace(&harness.namespace, "/hello/README.md").await;
    let readme_bytes = harness
        .namespace
        .read(readme.path, 0, u32::MAX)
        .await
        .expect("hello README read");
    assert!(
        !readme_bytes.bytes.is_empty(),
        "generated hello README remains readable"
    );
}

#[tokio::test]
async fn test_mutable_unversioned_full_reads_are_observation_only() {
    let harness = make_initialized_runtime(TEST_PROVIDER_CONFIG);

    let path = "/hello/fresh-full";
    let first = read_namespace(&harness.namespace, path).await.unwrap();
    assert_eq!(first.bytes.as_slice(), b"fresh-full-1\n");
    assert_eq!(
        first.attrs.stability,
        omnifs_engine::StabilityClass::Dynamic
    );
    let second = read_namespace(&harness.namespace, path).await.unwrap();
    assert_eq!(second.bytes.as_slice(), b"fresh-full-2\n");
}

#[tokio::test]
async fn test_read_file() {
    let harness = make_initialized_runtime(TEST_PROVIDER_CONFIG);
    let result = read_namespace(&harness.namespace, "/hello/message")
        .await
        .unwrap();
    assert_eq!(result.bytes.as_slice(), b"Hello, world!");

    let exact = read_namespace(&harness.namespace, "/hello/lazy")
        .await
        .unwrap();
    assert_eq!(exact.bytes.as_slice(), b"lazy\n");
}

#[tokio::test]
async fn test_read_file_sibling_projections_do_not_erase_parent_dirents() {
    let harness = make_initialized_runtime(TEST_PROVIDER_CONFIG);

    let _ = list_namespace(&harness.namespace, "/hello")
        .await
        .expect("expected list entries");

    let result = read_namespace(&harness.namespace, "/hello/projected")
        .await
        .unwrap();
    assert_eq!(result.bytes.as_slice(), b"title\n");

    let mut entry_names: Vec<_> = list_namespace(&harness.namespace, "/hello")
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    entry_names.sort_unstable();
    assert_eq!(
        entry_names,
        vec![
            "README.md",
            "bundle",
            "feed",
            "fresh-full",
            "greeting",
            "large-ranged",
            "lazy",
            "live-log",
            "message",
            "projected",
            "ranged",
            "remote-a",
            "remote-b",
            "snapshot",
            "throttled",
            "unbounded",
            "unknown-ranged",
            "volatile-tail"
        ]
    );
}

/// Regression (live FUSE collapse): looking up one child of an object directory
/// must not shrink the directory's cached listing to that single child.
///
/// `ls` of an object dir (here `/items/open/7`) lists every leaf. A later access
/// of one child (`cat body`) drives a `lookup_child`, which folds its
/// `target + siblings` into the parent's cached dirents. The object's leaf set is
/// statically known, so the lookup answers `exhaustive` and the host treats the
/// fold as the whole directory. The lookup therefore MUST carry every other leaf
/// as a sibling; otherwise the fold replaces the listing with just the
/// looked-up child and a subsequent readdir enumerates only `body`.
#[tokio::test]
async fn test_object_dir_child_lookup_preserves_full_listing() {
    let harness = make_initialized_runtime(TEST_PROVIDER_CONFIG);

    let object_dir = p("/items/open/7");
    // The conformance Item anchor exposes its file leaves (item.json/item.md +
    // title/state/body derives), a `log` object stream face, the `comments`
    // child-object collection dir, the tree-capable `checkout` dir, and
    // `replies` (a Comment alias subtree).
    let expected = vec![
        "body",
        "checkout",
        "comments",
        "item.json",
        "item.md",
        "log",
        "replies",
        "state",
        "title",
    ];

    // Cold `ls` of the object dir lists every leaf.
    let listing = list_namespace(&harness.namespace, object_dir.as_str())
        .await
        .unwrap();
    let mut cold_names: Vec<&str> = listing.iter().map(|e| e.name.as_str()).collect();
    cold_names.sort_unstable();
    assert_eq!(
        cold_names, expected,
        "cold listing must enumerate every leaf"
    );

    // `cat /items/open/7/body`: the lookup the FUSE/NFS path runs to resolve the
    // child before reading it.
    let lookup = resolve_namespace(&harness.namespace, "/items/open/7/body").await;
    assert_eq!(lookup.attrs.kind, EntryKind::File);

    // A subsequent readdir reads the cached dirents the lookup just folded into.
    let mut warm_names: Vec<String> = list_namespace(&harness.namespace, object_dir.as_str())
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    warm_names.sort_unstable();
    assert_eq!(
        warm_names, expected,
        "reading one child must not collapse the object dir's listing"
    );
}

#[tokio::test]
async fn test_ranged_open_read_chunk_contract() {
    let harness = make_initialized_runtime(TEST_PROVIDER_CONFIG);

    let ranged = resolve_namespace(&harness.namespace, "/hello/ranged").await;
    assert_eq!(ranged.attrs.read_style, omnifs_engine::ReadStyle::Ranged);

    let chunk = harness
        .namespace
        .read(ranged.path.clone(), 2, 4)
        .await
        .unwrap();
    assert_eq!(chunk.bytes, b"cdef");
    assert_eq!(chunk.attrs.size, 26);
    assert_eq!(
        chunk.attrs.stability,
        omnifs_engine::StabilityClass::Dynamic
    );
    assert!(!chunk.eof);

    let eof = harness
        .namespace
        .read(ranged.path.clone(), 26, 8)
        .await
        .unwrap();
    assert!(eof.bytes.is_empty());
    assert!(eof.eof);
}

#[tokio::test]
async fn test_unknown_and_volatile_ranged_eof_contracts() {
    let harness = make_initialized_runtime(TEST_PROVIDER_CONFIG);

    let unknown = resolve_namespace(&harness.namespace, "/hello/unknown-ranged").await;
    assert_eq!(unknown.attrs.size, 1);
    let eof = harness
        .namespace
        .read(unknown.path.clone(), 8, 32)
        .await
        .unwrap();
    assert_eq!(eof.bytes, b"size\n");
    assert!(eof.eof);
    assert_eq!(eof.attrs.size, 13);
    assert_eq!(
        harness.namespace.getattr(unknown.path).await.unwrap().size,
        13
    );
    let volatile = resolve_namespace(&harness.namespace, "/hello/volatile-tail").await;
    assert_eq!(volatile.attrs.read_style, omnifs_engine::ReadStyle::Ranged);
    assert_eq!(volatile.attrs.size, 1);
    let chunk = harness
        .namespace
        .read(volatile.path, 42, 128)
        .await
        .unwrap();
    assert_eq!(chunk.bytes, b"tail:42\n");
    assert_eq!(chunk.attrs.stability, omnifs_engine::StabilityClass::Live);
    assert!(!chunk.eof);
}

#[tokio::test]
async fn test_lookup_child() {
    let harness = make_runtime();
    let result = resolve_namespace(&harness.namespace, "/hello").await;
    assert_eq!(result.attrs.kind, EntryKind::Directory);
    let exact_file = resolve_namespace(&harness.namespace, "/hello/lazy").await;
    assert_eq!(exact_file.attrs.kind, EntryKind::File);

    assert_eq!(
        harness
            .namespace
            .getattr(p("/test/hello/lazy"))
            .await
            .unwrap()
            .kind,
        EntryKind::File
    );

    let hello = resolve_namespace(&harness.namespace, "/hello").await;
    let missing = harness
        .namespace
        .lookup(hello.path.clone(), "missing")
        .await;
    assert_eq!(missing, Err(NsError::NotFound));
}

#[tokio::test]
async fn test_subtree_handoff_rejects_unknown_tree_ref() {
    let harness = make_initialized_runtime(TEST_PROVIDER_CONFIG);

    let parent = resolve_namespace(&harness.namespace, "/items/open/7").await;
    let lookup_error = harness
        .namespace
        .lookup(parent.path.clone(), "checkout")
        .await
        .unwrap_err();
    assert!(
        lookup_error
            .to_string()
            .contains("subtree result references unknown tree 777"),
        "unexpected error: {lookup_error}"
    );

    let listing_error = harness
        .namespace
        .readdir(p("/test/items/open/7/checkout"), DirCursor::start(), 0)
        .await
        .unwrap_err();
    assert!(
        listing_error
            .to_string()
            .contains("subtree result references unknown tree 777"),
        "unexpected error: {listing_error}"
    );
}

#[tokio::test]
// Long integration test: two full runtimes built end to end. Splitting it
// buys nothing.
#[allow(clippy::too_many_lines)]
async fn test_cache_isolated_by_mount_name() {
    let harness = omnifs_itest::RuntimeHarness::new_multi(&[
        r#"{"provider":"test_provider.wasm","mount":"mount-a"}"#,
        r#"{"provider":"test_provider.wasm","mount":"mount-b"}"#,
    ])
    .unwrap();
    let ns = &harness.namespace;
    let root = ns
        .readdir(Path::root(), DirCursor::start(), 0)
        .await
        .unwrap();
    let mut root_names: Vec<_> = root
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    root_names.sort_unstable();
    assert_eq!(root_names, vec!["mount-a", "mount-b"]);
    let runtime_a = harness.registry.get("mount-a").unwrap();
    let _ = list_mount_namespace(ns, "mount-a", "/hello").await.unwrap();
    assert!(list_mount_namespace(ns, "mount-a", "/hello").await.is_ok());
    assert!(list_mount_namespace(ns, "mount-b", "/hello").await.is_ok());

    let _ = list_mount_namespace(ns, "mount-a", "/scoped")
        .await
        .unwrap();
    let _ = list_mount_namespace(ns, "mount-b", "/scoped")
        .await
        .unwrap();
    let item_a = resolve_mount_namespace(ns, "mount-a", "/scoped/item").await;
    let item_b = resolve_mount_namespace(ns, "mount-b", "/scoped/item").await;
    let mut events = ns.subscribe();
    let op_gen = runtime_a.resources.current_epoch();
    let (tick_result, effects) = harness
        .timer_tick()
        .unwrap()
        .into_result_and_effects()
        .unwrap();
    tick_result.unwrap();
    publish_effects_for_test(&runtime_a, &effects, op_gen).expect("timer effects should publish");
    let refreshed_a = ns.getattr(item_a.path.clone()).await.unwrap();
    assert_ne!(refreshed_a.change, item_a.attrs.change);
    let refreshed_b = ns.getattr(item_b.path.clone()).await.unwrap();
    assert_eq!(refreshed_b.change, item_b.attrs.change);
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
        .await
        .expect("mount-a invalidation event")
        .expect("namespace event stream remains open");
    assert!(matches!(
        event,
        omnifs_engine::NsEvent::InvalidateSubtree { path } if path == item_a.path
    ));
    while let Some(event) = events.try_recv() {
        if let omnifs_engine::NsEvent::InvalidateSubtree { path } = event {
            assert_eq!(path, item_a.path, "only mount-a should be invalidated");
        }
    }
}
