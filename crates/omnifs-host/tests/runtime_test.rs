use std::sync::Arc;

use omnifs_cache::Caches;
use omnifs_cache::{Record as CacheRecord, RecordKind};
use omnifs_core::path::Path as OmnifsPath;
use omnifs_core::view::{DirentsPayload, FilePayload, LookupPayload};
use omnifs_host::clock::now_millis;
use omnifs_host::cloner::GitCloner;
use omnifs_host::mounts::Spec;
use omnifs_host::{Dirs, LookupOutcome, Runtime};
use omnifs_itest::{
    inline_content, make_engine, make_extractor, make_initialized_runtime, make_runtime,
    provider_wasm_path,
};
use omnifs_wit::provider::types::{EntryKind, FileSize, ListChildrenResult, OpResult, Stability};

#[tokio::test]
async fn test_initialize() {
    let engine = make_engine();
    let harness = make_runtime(&engine);
    let info = harness.runtime.provider_info();
    assert_eq!(info.name, "test-provider");
    assert_eq!(info.version, "0.1.0");
}

#[tokio::test]
async fn test_list_root() {
    let engine = make_engine();
    let harness = make_runtime(&engine);
    let result = harness
        .runtime
        .namespace()
        .list_children("/", None, None, None)
        .await
        .unwrap();
    match result {
        ListChildrenResult::Entries(listing) => {
            assert_eq!(listing.entries.len(), 4);
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.contains(&"items"));
            assert!(names.contains(&"hello"));
            assert!(names.contains(&"scoped"));
            assert!(names.contains(&"checkout"));
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
        .list_children("/hello", None, None, None)
        .await
        .unwrap();
    match result {
        ListChildrenResult::Entries(listing) => {
            assert_eq!(listing.entries.len(), 13);
            let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&"message"));
            assert!(names.contains(&"greeting"));
            assert!(names.contains(&"projected"));
            assert!(names.contains(&"lazy"));
            assert!(names.contains(&"fresh-full"));
            assert!(names.contains(&"ranged"));
            assert!(names.contains(&"unknown-ranged"));
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
        .list_children("/hello", None, None, None)
        .await
        .unwrap();
    assert!(
        matches!(result, ListChildrenResult::Entries(_)),
        "expected list entries, got {result:?}"
    );

    let title = harness
        .runtime
        .cache_get("/hello/bundle/title", RecordKind::File, None)
        .expect("bundle title should be projected");
    let body = harness
        .runtime
        .cache_get("/hello/bundle/body", RecordKind::File, None)
        .expect("bundle body should be projected");
    let empty = harness
        .runtime
        .cache_get("/hello/bundle/empty", RecordKind::File, None)
        .expect("bundle empty file should be projected");
    let bundle_dirents = harness
        .runtime
        .cache_get("/hello/bundle", RecordKind::Dirents, None)
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
        .list_children("/hello/bundle", None, None, None)
        .await
        .unwrap();
    assert!(
        matches!(result, ListChildrenResult::Entries(_)),
        "expected DirEntries, got {result:?}"
    );

    let title = harness
        .runtime
        .cache_get("/hello/bundle/title", RecordKind::File, None)
        .expect("projected title should be cached at its own path");
    let body = harness
        .runtime
        .cache_get("/hello/bundle/body", RecordKind::File, None)
        .expect("projected body should be cached at its own path");

    assert_eq!(file_payload(&title).content, b"title".to_vec());
    assert_eq!(file_payload(&body).content, b"body".to_vec());
    assert!(
        harness
            .runtime
            .cache_get("/hello/bundle/title/title", RecordKind::File, None)
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
    let content_type = OmnifsPath::parse(path)
        .unwrap()
        .content_type_mime(None)
        .to_string();
    let first = harness
        .runtime
        .namespace()
        .read_file(path, content_type.clone(), None)
        .await
        .unwrap();
    assert_eq!(inline_content(&first), b"fresh-full-1\n");
    assert_eq!(first.attrs.stability, Stability::Mutable);
    assert_eq!(first.attrs.version_token, None);
    assert!(
        harness
            .runtime
            .cache_get(path, RecordKind::File, None)
            .is_none(),
        "unversioned mutable full-read bytes must not be durably cached",
    );

    let second = harness
        .runtime
        .namespace()
        .read_file(path, content_type, None)
        .await
        .unwrap();
    assert_eq!(inline_content(&second), b"fresh-full-2\n");
    assert!(
        harness
            .runtime
            .cache_get(path, RecordKind::File, None)
            .is_none(),
        "second unversioned mutable read must not create a durable file payload",
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
            "/hello/message",
            OmnifsPath::parse("/hello/message")
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
            "/hello/lazy",
            OmnifsPath::parse("/hello/lazy")
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
        .list_children("/hello", None, None, None)
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
            "/hello/projected",
            OmnifsPath::parse("/hello/projected")
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
        .cache_get("/hello", RecordKind::Dirents, None)
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
            "lazy",
            "message",
            "projected",
            "ranged",
            "snapshot",
            "throttled",
            "unbounded",
            "unknown-ranged",
            "volatile-tail"
        ]
    );

    let body = harness
        .runtime
        .cache_get("/hello/body", RecordKind::File, None)
        .expect("body sibling projection should be cached");
    let state = harness
        .runtime
        .cache_get("/hello/state", RecordKind::File, None)
        .expect("state sibling projection should be cached");
    assert_eq!(file_payload(&body).content, b"body\n");
    assert_eq!(file_payload(&state).content, b"open\n");
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
        .open_file("/hello/ranged")
        .await
        .unwrap();
    assert!(matches!(opened.attrs.size, FileSize::Exact(26)));
    assert_eq!(opened.attrs.stability, Stability::Mutable);
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
        .open_file("/hello/unknown-ranged")
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
        .open_file("/hello/volatile-tail")
        .await
        .unwrap();
    assert_eq!(opened.attrs.stability, Stability::Volatile);
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
        .lookup_child("/", "hello", None)
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
        .lookup_child("/hello", "lazy", None)
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
        .cache_get("/hello/lazy", RecordKind::Lookup, None)
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
        .lookup_child("/hello", "missing", None)
        .await
        .unwrap();
    assert!(
        matches!(missing, LookupOutcome::NotFound),
        "expected lookup miss, got {missing:?}"
    );

    assert!(
        harness
            .runtime
            .cache_get("/hello/missing", RecordKind::Lookup, None)
            .is_none(),
        "lookup miss must not create a non-expiring view-cache record"
    );
    assert!(
        harness
            .runtime
            .negative_for("/hello/missing", now_millis())
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
        .lookup_child("/", "checkout", None)
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
        .list_children("/checkout", None, None, None)
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
    // path-dispatch-and-listing: a bare `lookup` is light and does not warm
    // a child's adjacent shape; the preload a dir handler attaches with
    // `preload_*` lands when the directory is actually *listed*. Listing
    // `hello/bundle` runs the `bundle` handler, whose projection preloads
    // `title`/`body` alongside the listing.
    let listing = harness
        .runtime
        .namespace()
        .list_children("/hello/bundle", None, None, None)
        .await
        .unwrap();
    match &listing {
        ListChildrenResult::Entries(_) => {},
        other => panic!("expected list entries, got {other:?}"),
    }

    // Verify the projection effects were cached.
    let title = harness
        .runtime
        .cache_get("/hello/bundle/title", RecordKind::File, None)
        .expect("title should be in cache");
    let body = harness
        .runtime
        .cache_get("/hello/bundle/body", RecordKind::File, None)
        .expect("body should be in cache");
    let bundle_dirents = harness
        .runtime
        .cache_get("/hello/bundle", RecordKind::Dirents, None)
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
        .lookup_child("/hello", "snapshot", None)
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
        .cache_get("/hello", RecordKind::Dirents, None)
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
            "lazy",
            "message",
            "projected",
            "ranged",
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
        .list_children("/hello/snapshot", None, None, None)
        .await
        .unwrap();
    match &listing {
        ListChildrenResult::Entries(_) => {},
        other => panic!("expected list entries, got {other:?}"),
    }

    let dirents_record = harness
        .runtime
        .cache_get("/hello/snapshot", RecordKind::Dirents, None)
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
        .cache_get("/hello/snapshot/status", RecordKind::File, None)
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
        .cache_put("/owner/repo", RecordKind::Attr, None, &record);
    harness
        .runtime
        .cache_put("/owner/repo/issues", RecordKind::Attr, None, &record);
    harness
        .runtime
        .cache_put("/owner/repobaz", RecordKind::Attr, None, &record);

    harness.runtime.cache_delete_prefix("/owner/repo");

    assert!(
        harness
            .runtime
            .cache_get("/owner/repo", RecordKind::Attr, None)
            .is_none()
    );
    assert!(
        harness
            .runtime
            .cache_get("/owner/repo/issues", RecordKind::Attr, None)
            .is_none()
    );
    assert!(
        harness
            .runtime
            .cache_get("/owner/repobaz", RecordKind::Attr, None)
            .is_some()
    );
}

#[tokio::test]
// Long integration test: two full runtimes built end to end. Splitting it
// buys nothing.
#[allow(clippy::too_many_lines)]
async fn test_cache_isolated_by_mount_name() {
    let engine = make_engine();
    let config = Spec::parse(
        r#"
        {
            "provider": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    )
    .unwrap();

    let clone_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
    let extractor = make_extractor();
    let wasm_path = provider_wasm_path(&config.provider);
    let mut config_a = config.clone();
    config_a.mount = "mount-a".to_string();
    let mut config_b = config;
    config_b.mount = "mount-b".to_string();
    let resolved_a = config_a.into_resolved("test_provider", None).unwrap();
    let resolved_b = config_b.into_resolved("test_provider", None).unwrap();
    // Both runtimes share the same global Caches; mount isolation is via key prefix.
    let caches = Caches::open(cache_dir.path()).unwrap();
    let runtime_a = Runtime::new(
        &engine,
        &wasm_path,
        &resolved_a,
        cloner.clone(),
        Dirs::new(
            cache_dir.path(),
            config_dir.path(),
            config_dir.path(),
            config_dir.path(),
        ),
        extractor.clone(),
        &caches,
    )
    .unwrap();
    let runtime_b = Runtime::new(
        &engine,
        &wasm_path,
        &resolved_b,
        cloner,
        Dirs::new(
            cache_dir.path(),
            config_dir.path(),
            config_dir.path(),
            config_dir.path(),
        ),
        extractor,
        &caches,
    )
    .unwrap();

    let result = runtime_a
        .namespace()
        .list_children("/hello", None, None, None)
        .await
        .unwrap();
    assert!(matches!(result, ListChildrenResult::Entries(_)));
    assert!(
        runtime_a
            .cache_get("/hello", RecordKind::Dirents, None)
            .is_some()
    );
    assert!(
        runtime_b
            .cache_get("/hello", RecordKind::Dirents, None)
            .is_none()
    );

    let scoped_a = runtime_a
        .namespace()
        .list_children("/scoped", None, None, None)
        .await
        .unwrap();
    let scoped_b = runtime_b
        .namespace()
        .list_children("/scoped", None, None, None)
        .await
        .unwrap();
    assert!(matches!(scoped_a, ListChildrenResult::Entries(_)));
    assert!(matches!(scoped_b, ListChildrenResult::Entries(_)));
    assert!(
        runtime_a
            .cache_get("/scoped/item", RecordKind::Lookup, None)
            .is_some()
    );
    assert!(
        runtime_b
            .cache_get("/scoped/item", RecordKind::Lookup, None)
            .is_some()
    );

    let tick = runtime_a.call_timer_tick().await.unwrap();
    assert!(matches!(tick, OpResult::OnEvent));
    assert!(
        runtime_a
            .cache_get("/scoped/item", RecordKind::Lookup, None)
            .is_none()
    );
    assert!(
        runtime_b
            .cache_get("/scoped/item", RecordKind::Lookup, None)
            .is_some()
    );
    assert!(
        runtime_a
            .drain_invalidated_paths()
            .into_iter()
            .any(|path| path == "/scoped/item")
    );
}
