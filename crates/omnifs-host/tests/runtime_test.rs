use std::sync::Arc;

use omnifs_cache::Caches;
use omnifs_cache::{Record as CacheRecord, RecordKind};
use omnifs_core::path::Path;
use omnifs_core::view::{DirentsPayload, FilePayload, LookupPayload};
use omnifs_host::clock::now_millis;
use omnifs_host::cloner::GitCloner;
use omnifs_host::{HostContext, LookupOutcome, Runtime};
use omnifs_itest::{
    inline_content, make_engine, make_initialized_runtime, make_runtime, provider_wasm_path,
    spec_with_test_provider,
};
use omnifs_wit::provider::types::{EntryKind, FileSize, ListChildrenResult, OpResult, Stability};

fn p(value: &str) -> Path {
    Path::parse(value).unwrap()
}

fn test_context(
    cache_dir: &std::path::Path,
    config_dir: &std::path::Path,
    providers_dir: &std::path::Path,
    credentials_file: &std::path::Path,
) -> HostContext {
    HostContext::new(cache_dir, config_dir, providers_dir, credentials_file)
}

/// Every shipped provider must initialize (run `start()` + `seal()`) cleanly.
/// The seal runs inside `initialize` and needs no credentials, so this is a
/// deterministic gate for route-overlap and registration errors that a
/// `cargo check` for `wasm32-wasip2` cannot catch (it compiles but never seals).
/// This guards against the class of bug where a migrated provider mounts an
/// object at the wrong template (e.g. an object at `/{a}/{b}` colliding with a
/// literal route), which otherwise only surfaces at live mount time.
#[tokio::test]
async fn all_providers_initialize_and_seal() {
    // Providers whose `start()` registers routes without touching a backing
    // resource. `db` is excluded: it opens its SQLite file at init, so a bare
    // harness (no fixture) fails with an environmental I/O error, not a seal
    // error; db's seal is exercised through its live mount instead.
    let providers = [
        ("omnifs_provider_github.wasm", "github"),
        ("omnifs_provider_arxiv.wasm", "arxiv"),
        ("omnifs_provider_dns.wasm", "dns"),
        ("omnifs_provider_docker.wasm", "docker"),
        ("omnifs_provider_kubernetes.wasm", "k8s"),
        ("omnifs_provider_linear.wasm", "linear"),
        ("omnifs_provider_oura.wasm", "oura"),
    ];
    for (wasm, mount) in providers {
        let config = format!(r#"{{"provider":"{wasm}","mount":"{mount}"}}"#);
        let result = omnifs_itest::try_make_runtime_from_config(&config);
        assert!(
            result.is_ok(),
            "provider {wasm} failed to initialize/seal: {:?}",
            result.err()
        );
    }
}

#[tokio::test]
async fn test_list_root() {
    let engine = make_engine();
    let harness = make_runtime(&engine);
    let result = harness
        .runtime
        .namespace()
        .list_children(&p("/"), None, None, None)
        .await
        .unwrap();
    match result {
        ListChildrenResult::Entries(listing) => {
            assert_eq!(listing.entries.len(), 6);
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.contains(&"items"));
            assert!(names.contains(&"hello"));
            assert!(names.contains(&"scoped"));
            assert!(names.contains(&"dynamic"));
            assert!(names.contains(&"checkout"));
            assert!(names.contains(&"slow"));
            assert!(
                listing
                    .entries
                    .iter()
                    .all(|entry| matches!(entry.kind, EntryKind::Directory))
            );
        },
        other => panic!("expected list entries, got {other:?}"),
    }
}

#[tokio::test]
async fn test_list_hello_dir() {
    let engine = make_engine();
    let harness = make_runtime(&engine);
    let result = harness
        .runtime
        .namespace()
        .list_children(&p("/hello"), None, None, None)
        .await
        .unwrap();
    match result {
        ListChildrenResult::Entries(listing) => {
            assert_eq!(listing.entries.len(), 16);
            let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
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
            assert!(names.contains(&"bundle"));
            assert!(names.contains(&"feed"));
            assert!(names.contains(&"snapshot"));
            assert!(names.contains(&"throttled"));
            assert!(names.contains(&"unbounded"));
        },
        other => panic!("expected list entries, got {other:?}"),
    }
}

#[tokio::test]
async fn test_list_projects_nested_files_into_cache() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let result = harness
        .runtime
        .namespace()
        .list_children(&p("/hello"), None, None, None)
        .await
        .unwrap();
    assert!(
        matches!(result, ListChildrenResult::Entries(_)),
        "expected list entries, got {result:?}"
    );

    let title = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/bundle/title"), RecordKind::File, None)
        .expect("bundle title should be projected");
    let body = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/bundle/body"), RecordKind::File, None)
        .expect("bundle body should be projected");
    let empty = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/bundle/empty"), RecordKind::File, None)
        .expect("bundle empty file should be projected");
    let bundle_dirents = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/bundle"), RecordKind::Dirents, None)
        .expect("bundle dirents should be projected");
    assert_eq!(file_payload(&title).content, b"title".to_vec());
    assert_eq!(file_payload(&body).content, b"body".to_vec());
    assert!(file_payload(&empty).content.is_empty());
    let dirents = DirentsPayload::deserialize(&bundle_dirents.payload)
        .expect("bundle dirents payload should deserialize");
    let mut entry_names: Vec<_> = dirents
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    entry_names.sort_unstable();
    assert_eq!(entry_names, vec!["body", "empty", "title"]);
    assert!(!dirents.exhaustive);
}

#[tokio::test]
async fn test_list_projects_direct_file_content_into_cache() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let result = harness
        .runtime
        .namespace()
        .list_children(&p("/hello/bundle"), None, None, None)
        .await
        .unwrap();
    assert!(
        matches!(result, ListChildrenResult::Entries(_)),
        "expected DirEntries, got {result:?}"
    );

    let title = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/bundle/title"), RecordKind::File, None)
        .expect("projected title should be cached at its own path");
    let body = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/bundle/body"), RecordKind::File, None)
        .expect("projected body should be cached at its own path");

    assert_eq!(file_payload(&title).content, b"title".to_vec());
    assert_eq!(file_payload(&body).content, b"body".to_vec());
    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p("/hello/bundle/title/title"), RecordKind::File, None)
            .is_none(),
        "projected file content must not be nested under itself"
    );
}

#[tokio::test]
async fn test_mutable_unversioned_full_reads_are_observation_only() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let path = "/hello/fresh-full";
    let content_type = Path::parse(path)
        .unwrap()
        .content_type_mime(None)
        .to_string();
    let first = harness
        .runtime
        .namespace()
        .read_file(&p(path), content_type.clone(), None)
        .await
        .unwrap();
    assert_eq!(inline_content(&first), b"fresh-full-1\n");
    assert_eq!(first.attrs.stability, Stability::Dynamic);
    assert_eq!(first.attrs.version_token, None);
    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p(path), RecordKind::File, None)
            .is_none(),
        "unversioned dynamic full-read bytes must not be durably cached",
    );

    let second = harness
        .runtime
        .namespace()
        .read_file(&p(path), content_type, None)
        .await
        .unwrap();
    assert_eq!(inline_content(&second), b"fresh-full-2\n");
    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p(path), RecordKind::File, None)
            .is_none(),
        "second unversioned dynamic read must not create a durable file payload",
    );
}

#[tokio::test]
async fn test_read_file() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );
    let result = harness
        .runtime
        .namespace()
        .read_file(
            &p("/hello/message"),
            Path::parse("/hello/message")
                .unwrap()
                .content_type_mime(None)
                .to_string(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(inline_content(&result), b"Hello, world!");

    let exact = harness
        .runtime
        .namespace()
        .read_file(
            &p("/hello/lazy"),
            Path::parse("/hello/lazy")
                .unwrap()
                .content_type_mime(None)
                .to_string(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(inline_content(&exact), b"lazy\n");
}

#[tokio::test]
async fn test_read_file_sibling_projections_do_not_erase_parent_dirents() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let listing = harness
        .runtime
        .namespace()
        .list_children(&p("/hello"), None, None, None)
        .await
        .unwrap();
    match listing {
        ListChildrenResult::Entries(_) => {},
        other => panic!("expected list entries, got {other:?}"),
    }

    let result = harness
        .runtime
        .namespace()
        .read_file(
            &p("/hello/projected"),
            Path::parse("/hello/projected")
                .unwrap()
                .content_type_mime(None)
                .to_string(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(inline_content(&result), b"title\n");

    let dirents_record = harness
        .runtime
        .cache()
        .cache_get(&p("/hello"), RecordKind::Dirents, None)
        .expect("hello dirents should stay cached");
    let dirents = DirentsPayload::deserialize(&dirents_record.payload)
        .expect("dirents payload should deserialize");
    let mut entry_names: Vec<_> = dirents
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    entry_names.sort_unstable();
    assert_eq!(
        entry_names,
        vec![
            "bundle",
            "feed",
            "fresh-full",
            "greeting",
            "large-ranged",
            "lazy",
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

    let body = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/body"), RecordKind::File, None)
        .expect("body sibling projection should be cached");
    let state = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/state"), RecordKind::File, None)
        .expect("state sibling projection should be cached");
    assert_eq!(file_payload(&body).content, b"body\n");
    assert_eq!(file_payload(&state).content, b"open\n");
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
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": { "domains": ["httpbin.org"] }
        }
    "#,
    );

    let object_dir = p("/items/open/7");
    // The conformance Item anchor exposes its file leaves (item.json/item.md +
    // title/state/body derives), a `log` object stream face, the `comments`
    // child-object collection dir, and `replies` (a Comment alias subtree).
    let expected = vec![
        "body",
        "comments",
        "item.json",
        "item.md",
        "log",
        "replies",
        "state",
        "title",
    ];

    // Cold `ls` of the object dir lists every leaf.
    let listing = harness
        .runtime
        .namespace()
        .list_children(&object_dir, None, None, None)
        .await
        .unwrap();
    let ListChildrenResult::Entries(listing) = listing else {
        panic!("expected list entries");
    };
    let mut cold_names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    cold_names.sort_unstable();
    assert_eq!(
        cold_names, expected,
        "cold listing must enumerate every leaf"
    );

    // `cat /items/open/7/body`: the lookup the FUSE/NFS path runs to resolve the
    // child before reading it.
    let lookup = harness
        .runtime
        .namespace()
        .lookup_child(&object_dir, "body", None)
        .await
        .unwrap();
    match &lookup {
        LookupOutcome::Entry(entry) => assert_eq!(entry.path().as_str(), "/items/open/7/body"),
        other => panic!("expected lookup entry, got {other:?}"),
    }

    // A subsequent readdir reads the cached dirents the lookup just folded into.
    let dirents_record = harness
        .runtime
        .cache()
        .cache_get(&object_dir, RecordKind::Dirents, None)
        .expect("object dir dirents must stay cached");
    let dirents = DirentsPayload::deserialize(&dirents_record.payload)
        .expect("dirents payload should deserialize");
    let mut warm_names: Vec<&str> = dirents.entries.iter().map(|e| e.name.as_str()).collect();
    warm_names.sort_unstable();
    assert_eq!(
        warm_names, expected,
        "reading one child must not collapse the object dir's listing"
    );
}

#[tokio::test]
async fn test_ranged_open_read_chunk_contract() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let opened = harness
        .runtime
        .namespace()
        .open_file(&p("/hello/ranged"))
        .await
        .unwrap();
    assert!(matches!(opened.attrs.size, FileSize::Exact(26)));
    assert_eq!(opened.attrs.stability, Stability::Dynamic);
    assert_eq!(opened.attrs.version_token.as_deref(), Some("alphabet-v1"));

    let chunk = harness
        .runtime
        .namespace()
        .read_chunk(opened.handle, 2, 4)
        .await
        .unwrap();
    assert_eq!(chunk.content, b"cdef");
    assert!(!chunk.eof);

    let eof = harness
        .runtime
        .namespace()
        .read_chunk(opened.handle, 26, 8)
        .await
        .unwrap();
    assert!(eof.content.is_empty());
    assert!(eof.eof);

    harness.runtime.call_close_file(opened.handle).unwrap();
}

#[tokio::test]
async fn test_unknown_and_volatile_ranged_eof_contracts() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let opened = harness
        .runtime
        .namespace()
        .open_file(&p("/hello/unknown-ranged"))
        .await
        .unwrap();
    assert!(matches!(opened.attrs.size, FileSize::Unknown));
    let eof = harness
        .runtime
        .namespace()
        .read_chunk(opened.handle, 8, 32)
        .await
        .unwrap();
    assert_eq!(eof.content, b"size\n");
    assert!(eof.eof);
    harness.runtime.call_close_file(opened.handle).unwrap();

    let opened = harness
        .runtime
        .namespace()
        .open_file(&p("/hello/volatile-tail"))
        .await
        .unwrap();
    assert_eq!(opened.attrs.stability, Stability::Live);
    assert!(matches!(opened.attrs.size, FileSize::Unknown));
    let chunk = harness
        .runtime
        .namespace()
        .read_chunk(opened.handle, 42, 128)
        .await
        .unwrap();
    assert_eq!(chunk.content, b"tail:42\n");
    assert!(!chunk.eof);
    harness.runtime.call_close_file(opened.handle).unwrap();
}

#[tokio::test]
async fn test_lookup_child() {
    let engine = make_engine();
    let harness = make_runtime(&engine);
    let result = harness
        .runtime
        .namespace()
        .lookup_child(&p("/"), "hello", None)
        .await
        .unwrap();
    match result {
        LookupOutcome::Entry(entry) => {
            assert_eq!(entry.path().as_str(), "/hello");
            assert!(entry.meta().is_directory());
        },
        other => panic!("expected Lookup, got {other:?}"),
    }

    let exact_file = harness
        .runtime
        .namespace()
        .lookup_child(&p("/hello"), "lazy", None)
        .await
        .unwrap();
    match exact_file {
        LookupOutcome::Entry(entry) => {
            assert_eq!(entry.path().as_str(), "/hello/lazy");
            assert!(entry.meta().is_file());
        },
        other => panic!("expected file Lookup, got {other:?}"),
    }

    let cached_lookup = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/lazy"), RecordKind::Lookup, None)
        .expect("lookup entry should be materialized");
    assert!(
        matches!(
            LookupPayload::deserialize(&cached_lookup.payload),
            Some(LookupPayload::Positive(_))
        ),
        "lookup entry should cache a positive record"
    );

    let missing = harness
        .runtime
        .namespace()
        .lookup_child(&p("/hello"), "missing", None)
        .await
        .unwrap();
    assert!(
        matches!(missing, LookupOutcome::NotFound),
        "expected lookup miss, got {missing:?}"
    );

    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p("/hello/missing"), RecordKind::Lookup, None)
            .is_none(),
        "lookup miss must not create a non-expiring view-cache record"
    );
    assert!(
        harness
            .runtime
            .cache()
            .negative_for(&p("/hello/missing"), now_millis())
            .is_some(),
        "lookup miss should update the live negative index"
    );
}

#[tokio::test]
async fn test_subtree_handoff_rejects_unknown_tree_ref() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let lookup_error = harness
        .runtime
        .namespace()
        .lookup_child(&p("/"), "checkout", None)
        .await
        .unwrap_err();
    assert!(
        lookup_error
            .to_string()
            .contains("subtree result references unknown tree 777"),
        "unexpected error: {lookup_error}"
    );

    let listing_error = harness
        .runtime
        .namespace()
        .list_children(&p("/checkout"), None, None, None)
        .await
        .unwrap_err();
    assert!(
        listing_error
            .to_string()
            .contains("subtree result references unknown tree 777"),
        "unexpected error: {listing_error}"
    );
}

/// Test that lookup-adjacent file projections are cached.
#[tokio::test]
async fn test_list_projects_adjacent_files_into_cache() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );
    // Projection-tree contract: a bare `lookup` is light and does not warm
    // a child's adjacent shape; the preload a dir handler attaches with
    // `preload_*` lands when the directory is actually *listed*. Listing
    // `hello/bundle` runs the `bundle` handler, whose projection preloads
    // `title`/`body` alongside the listing.
    let listing = harness
        .runtime
        .namespace()
        .list_children(&p("/hello/bundle"), None, None, None)
        .await
        .unwrap();
    match &listing {
        ListChildrenResult::Entries(_) => {},
        other => panic!("expected list entries, got {other:?}"),
    }

    // Verify the projection effects were cached.
    let title = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/bundle/title"), RecordKind::File, None)
        .expect("title should be in cache");
    let body = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/bundle/body"), RecordKind::File, None)
        .expect("body should be in cache");
    let bundle_dirents = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/bundle"), RecordKind::Dirents, None)
        .expect("bundle dirents should be in cache");

    assert_eq!(file_payload(&title).content, b"title".to_vec());
    assert_eq!(file_payload(&body).content, b"body".to_vec());
    let dirents = DirentsPayload::deserialize(&bundle_dirents.payload)
        .expect("bundle dirents payload should deserialize");
    let mut entry_names: Vec<_> = dirents
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    entry_names.sort_unstable();
    assert_eq!(entry_names, vec!["body", "title"]);
}

#[tokio::test]
async fn test_lookup_returns_siblings_and_list_warms_child_shape() {
    let harness = make_initialized_runtime(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );
    // a lookup materializes the target plus the parent's static sibling set,
    // but does NOT warm the child's shape (lookup is light).
    let result = harness
        .runtime
        .namespace()
        .lookup_child(&p("/hello"), "snapshot", None)
        .await
        .unwrap();

    match &result {
        LookupOutcome::Entry(entry) => {
            assert_eq!(entry.path().as_str(), "/hello/snapshot");
            assert!(entry.meta().is_directory());
        },
        other => panic!("expected Lookup, got {other:?}"),
    }

    let parent_dirents = harness
        .runtime
        .cache()
        .cache_get(&p("/hello"), RecordKind::Dirents, None)
        .expect("lookup should materialize parent dirents");
    let parent_dirents =
        DirentsPayload::deserialize(&parent_dirents.payload).expect("dirents must deserialize");
    let mut lookup_names: Vec<_> = parent_dirents
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    lookup_names.sort_unstable();
    assert_eq!(
        lookup_names,
        vec![
            "bundle",
            "feed",
            "fresh-full",
            "greeting",
            "large-ranged",
            "lazy",
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
    // The child's shape and the preload it attaches warm when the directory is
    // *listed*, not on the bare lookup above.
    let listing = harness
        .runtime
        .namespace()
        .list_children(&p("/hello/snapshot"), None, None, None)
        .await
        .unwrap();
    match &listing {
        ListChildrenResult::Entries(_) => {},
        other => panic!("expected list entries, got {other:?}"),
    }

    let dirents_record = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/snapshot"), RecordKind::Dirents, None)
        .expect("snapshot dirents should be cached");
    let dirents = DirentsPayload::deserialize(&dirents_record.payload)
        .expect("dirents payload should deserialize");
    let mut entry_names: Vec<_> = dirents
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    entry_names.sort_unstable();
    assert_eq!(entry_names, vec!["comments", "status"]);
    assert!(
        dirents.exhaustive,
        "snapshot's handler returns an exhaustive listing, so a subsequent \
         readdir must hit the cache without re-invoking list_children",
    );

    // The `status` file the handler preloads is cached alongside the listing.
    let status = harness
        .runtime
        .cache()
        .cache_get(&p("/hello/snapshot/status"), RecordKind::File, None)
        .expect("status file preload should be cached");
    assert_eq!(file_payload(&status).content, b"open\n");
}

fn file_payload(record: &CacheRecord) -> FilePayload {
    FilePayload::deserialize(&record.payload).expect("file payload should deserialize")
}

#[test]
fn cache_delete_prefix_respects_segment_boundaries() {
    let engine = make_engine();
    let harness = make_runtime(&engine);
    let record = CacheRecord::new(RecordKind::Attr, vec![1, 2, 3]);

    harness
        .runtime
        .cache()
        .cache_put(&p("/owner/repo"), RecordKind::Attr, None, &record);
    harness
        .runtime
        .cache()
        .cache_put(&p("/owner/repo/issues"), RecordKind::Attr, None, &record);
    harness
        .runtime
        .cache()
        .cache_put(&p("/owner/repobaz"), RecordKind::Attr, None, &record);

    harness.runtime.cache_delete_prefix(&p("/owner/repo"));

    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p("/owner/repo"), RecordKind::Attr, None)
            .is_none()
    );
    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p("/owner/repo/issues"), RecordKind::Attr, None)
            .is_none()
    );
    assert!(
        harness
            .runtime
            .cache()
            .cache_get(&p("/owner/repobaz"), RecordKind::Attr, None)
            .is_some()
    );
}

#[tokio::test]
// Long integration test: two full runtimes built end to end. Splitting it
// buys nothing.
#[allow(clippy::too_many_lines)]
async fn test_cache_isolated_by_mount_name() {
    let engine = make_engine();
    let config = spec_with_test_provider(
        r#"{ "mount": "test", "capabilities": { "domains": ["httpbin.org"] } }"#,
    );

    let clone_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
    let wasm_path = provider_wasm_path("test_provider.wasm");
    let mut config_a = config.clone();
    config_a.mount = "mount-a".to_string();
    let mut config_b = config;
    config_b.mount = "mount-b".to_string();
    // Both runtimes share the same global Caches; mount isolation is via key prefix.
    let caches = Caches::open(cache_dir.path()).unwrap();
    let paths = omnifs_workspace::layout::WorkspaceLayout::under_root(config_dir.path());
    let context_a = test_context(
        cache_dir.path(),
        &paths.config_dir,
        config_dir.path(),
        &paths.credentials_file,
    );
    let context_b = test_context(
        cache_dir.path(),
        &paths.config_dir,
        config_dir.path(),
        &paths.credentials_file,
    );
    let runtime_a = Runtime::new(
        &engine,
        &wasm_path,
        &config_a,
        cloner.clone(),
        &context_a,
        &caches,
    )
    .unwrap();
    let runtime_b =
        Runtime::new(&engine, &wasm_path, &config_b, cloner, &context_b, &caches).unwrap();

    let result = runtime_a
        .namespace()
        .list_children(&p("/hello"), None, None, None)
        .await
        .unwrap();
    assert!(matches!(result, ListChildrenResult::Entries(_)));
    assert!(
        runtime_a
            .cache()
            .cache_get(&p("/hello"), RecordKind::Dirents, None)
            .is_some()
    );
    assert!(
        runtime_b
            .cache()
            .cache_get(&p("/hello"), RecordKind::Dirents, None)
            .is_none()
    );

    let scoped_a = runtime_a
        .namespace()
        .list_children(&p("/scoped"), None, None, None)
        .await
        .unwrap();
    let scoped_b = runtime_b
        .namespace()
        .list_children(&p("/scoped"), None, None, None)
        .await
        .unwrap();
    assert!(matches!(scoped_a, ListChildrenResult::Entries(_)));
    assert!(matches!(scoped_b, ListChildrenResult::Entries(_)));
    assert!(
        runtime_a
            .cache()
            .cache_get(&p("/scoped/item"), RecordKind::Lookup, None)
            .is_some()
    );
    assert!(
        runtime_b
            .cache()
            .cache_get(&p("/scoped/item"), RecordKind::Lookup, None)
            .is_some()
    );

    let tick = runtime_a.call_timer_tick().await.unwrap();
    assert!(matches!(tick, OpResult::OnEvent));
    assert!(
        runtime_a
            .cache()
            .cache_get(&p("/scoped/item"), RecordKind::Lookup, None)
            .is_none()
    );
    assert!(
        runtime_b
            .cache()
            .cache_get(&p("/scoped/item"), RecordKind::Lookup, None)
            .is_some()
    );
    assert!(
        runtime_a
            .drain_invalidated_paths()
            .into_iter()
            .any(|path| path.as_str() == "/scoped/item")
    );
}
