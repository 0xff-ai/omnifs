mod support;

use std::sync::Arc;

use omnifs_host::cache::{
    AttrPayload, CacheRecord, DirentsPayload, EntryKindCache, FilePayload, LookupPayload,
    RecordKind,
};
use omnifs_host::config::InstanceConfig;
use omnifs_host::omnifs::provider::types::{
    EntryKind, FileSize, ListResult, LookupResult, OpResult, Stability,
};
use omnifs_host::runtime::CalloutRuntime;
use omnifs_host::runtime::cloner::GitCloner;
use support::{make_engine, make_initialized_runtime, make_runtime};

#[tokio::test]
async fn test_initialize() {
    let engine = make_engine();
    let harness = make_runtime(&engine);
    let result = harness.runtime.initialize().unwrap();
    match result {
        OpResult::Init(info) => {
            assert_eq!(info.name, "test-provider");
            assert_eq!(info.version, "0.1.0");
        },
        other => panic!("expected ProviderInitialized, got {other:?}"),
    }
}

#[tokio::test]
async fn test_list_root() {
    let engine = make_engine();
    let harness = make_runtime(&engine);
    harness.runtime.initialize().unwrap();
    let result = harness.runtime.call_list_children("").await.unwrap();
    match result {
        OpResult::List(ListResult::Entries(listing)) => {
            assert_eq!(listing.entries.len(), 3);
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
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
        other => panic!("expected DirEntries, got {other:?}"),
    }
}

#[tokio::test]
async fn test_list_hello_dir() {
    let engine = make_engine();
    let harness = make_runtime(&engine);
    harness.runtime.initialize().unwrap();
    let result = harness.runtime.call_list_children("hello").await.unwrap();
    match result {
        OpResult::List(ListResult::Entries(listing)) => {
            assert_eq!(listing.entries.len(), 9);
            let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&"message"));
            assert!(names.contains(&"greeting"));
            assert!(names.contains(&"projected"));
            assert!(names.contains(&"lazy"));
            assert!(names.contains(&"ranged"));
            assert!(names.contains(&"unknown-ranged"));
            assert!(names.contains(&"volatile-tail"));
            assert!(names.contains(&"bundle"));
            assert!(names.contains(&"snapshot"));
        },
        other => panic!("expected DirEntries, got {other:?}"),
    }
}

#[tokio::test]
async fn test_list_preloads_nested_files_into_cache() {
    let harness = make_initialized_runtime(
        r#"
        {
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let result = harness.runtime.call_list_children("hello").await.unwrap();
    assert!(
        matches!(result, OpResult::List(ListResult::Entries(_))),
        "expected DirEntries, got {result:?}"
    );

    let title = harness
        .runtime
        .cache_get("hello/bundle/title", RecordKind::File)
        .expect("bundle title should be preloaded");
    let body = harness
        .runtime
        .cache_get("hello/bundle/body", RecordKind::File)
        .expect("bundle body should be preloaded");
    let empty = harness
        .runtime
        .cache_get("hello/bundle/empty", RecordKind::File)
        .expect("bundle empty file should be preloaded");
    let bundle_dirents = harness
        .runtime
        .cache_get("hello/bundle", RecordKind::Dirents)
        .expect("bundle dirents should be preloaded");
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
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let result = harness
        .runtime
        .call_list_children("hello/bundle")
        .await
        .unwrap();
    assert!(
        matches!(result, OpResult::List(ListResult::Entries(_))),
        "expected DirEntries, got {result:?}"
    );

    let title = harness
        .runtime
        .cache_get("hello/bundle/title", RecordKind::File)
        .expect("projected title should be cached at its own path");
    let body = harness
        .runtime
        .cache_get("hello/bundle/body", RecordKind::File)
        .expect("projected body should be cached at its own path");

    assert_eq!(file_payload(&title).content, b"title".to_vec());
    assert_eq!(file_payload(&body).content, b"body".to_vec());
    assert!(
        harness
            .runtime
            .cache_get("hello/bundle/title/title", RecordKind::File)
            .is_none(),
        "projected file content must not be nested under itself"
    );
}

#[tokio::test]
async fn test_read_file() {
    let harness = make_initialized_runtime(
        r#"
        {
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );
    let result = harness
        .runtime
        .call_read_file("hello/message")
        .await
        .unwrap();
    match result {
        OpResult::Read(file_result) => {
            assert_eq!(support::inline_content(&file_result), b"Hello, world!");
        },
        other => panic!("expected File, got {other:?}"),
    }

    let exact = harness.runtime.call_read_file("hello/lazy").await.unwrap();
    match exact {
        OpResult::Read(file_result) => {
            assert_eq!(support::inline_content(&file_result), b"lazy\n");
        },
        other => panic!("expected exact File, got {other:?}"),
    }
}

#[tokio::test]
async fn test_read_file_sibling_projections_do_not_erase_parent_dirents() {
    let harness = make_initialized_runtime(
        r#"
        {
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let listing = harness.runtime.call_list_children("hello").await.unwrap();
    match listing {
        OpResult::List(ListResult::Entries(_)) => {},
        other => panic!("expected DirEntries, got {other:?}"),
    }

    let result = harness
        .runtime
        .call_read_file("hello/projected")
        .await
        .unwrap();
    match result {
        OpResult::Read(file_result) => {
            assert_eq!(support::inline_content(&file_result), b"title\n");
        },
        other => panic!("expected File, got {other:?}"),
    }

    let dirents_record = harness
        .runtime
        .cache_get("hello", RecordKind::Dirents)
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
            "greeting",
            "lazy",
            "message",
            "projected",
            "ranged",
            "snapshot",
            "unknown-ranged",
            "volatile-tail"
        ]
    );

    let body = harness
        .runtime
        .cache_get("hello/body", RecordKind::File)
        .expect("body sibling projection should be cached");
    let state = harness
        .runtime
        .cache_get("hello/state", RecordKind::File)
        .expect("state sibling projection should be cached");
    assert_eq!(file_payload(&body).content, b"body\n");
    assert_eq!(file_payload(&state).content, b"open\n");
}

#[tokio::test]
async fn test_ranged_open_read_chunk_contract() {
    let harness = make_initialized_runtime(
        r#"
        {
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let open = harness
        .runtime
        .call_open_file("hello/ranged")
        .await
        .unwrap();
    let OpResult::OpenFile(opened) = open else {
        panic!("expected OpenFile, got {open:?}");
    };
    assert!(matches!(opened.attrs.size, FileSize::Exact(26)));
    assert_eq!(opened.attrs.stability, Stability::Mutable);
    assert_eq!(opened.attrs.version_token.as_deref(), Some("alphabet-v1"));

    let chunk = harness
        .runtime
        .call_read_chunk(opened.handle, 2, 4)
        .await
        .unwrap();
    let OpResult::ReadChunk(chunk) = chunk else {
        panic!("expected ReadChunk, got {chunk:?}");
    };
    assert_eq!(chunk.content, b"cdef");
    assert!(!chunk.eof);

    let eof = harness
        .runtime
        .call_read_chunk(opened.handle, 26, 8)
        .await
        .unwrap();
    let OpResult::ReadChunk(eof) = eof else {
        panic!("expected ReadChunk EOF, got {eof:?}");
    };
    assert!(eof.content.is_empty());
    assert!(eof.eof);

    harness.runtime.call_close_file(opened.handle).unwrap();
}

#[tokio::test]
async fn test_unknown_and_volatile_ranged_eof_contracts() {
    let harness = make_initialized_runtime(
        r#"
        {
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let open = harness
        .runtime
        .call_open_file("hello/unknown-ranged")
        .await
        .unwrap();
    let OpResult::OpenFile(opened) = open else {
        panic!("expected OpenFile, got {open:?}");
    };
    assert!(matches!(opened.attrs.size, FileSize::Unknown));
    let eof = harness
        .runtime
        .call_read_chunk(opened.handle, 8, 32)
        .await
        .unwrap();
    let OpResult::ReadChunk(eof) = eof else {
        panic!("expected ReadChunk EOF, got {eof:?}");
    };
    assert_eq!(eof.content, b"size\n");
    assert!(eof.eof);
    harness.runtime.call_close_file(opened.handle).unwrap();

    let open = harness
        .runtime
        .call_open_file("hello/volatile-tail")
        .await
        .unwrap();
    let OpResult::OpenFile(opened) = open else {
        panic!("expected OpenFile, got {open:?}");
    };
    assert_eq!(opened.attrs.stability, Stability::Volatile);
    assert!(matches!(opened.attrs.size, FileSize::Unknown));
    let chunk = harness
        .runtime
        .call_read_chunk(opened.handle, 42, 128)
        .await
        .unwrap();
    let OpResult::ReadChunk(chunk) = chunk else {
        panic!("expected live ReadChunk, got {chunk:?}");
    };
    assert_eq!(chunk.content, b"tail:42\n");
    assert!(!chunk.eof);
    harness.runtime.call_close_file(opened.handle).unwrap();
}

#[tokio::test]
async fn test_lookup_child() {
    let engine = make_engine();
    let harness = make_runtime(&engine);
    harness.runtime.initialize().unwrap();
    let result = harness
        .runtime
        .call_lookup_child("", "hello")
        .await
        .unwrap();
    match result {
        OpResult::Lookup(LookupResult::Entry(result)) => {
            let entry = &result.target;
            assert_eq!(entry.name, "hello");
            assert!(matches!(entry.kind, EntryKind::Directory));
        },
        other => panic!("expected Lookup, got {other:?}"),
    }

    let exact_file = harness
        .runtime
        .call_lookup_child("hello", "lazy")
        .await
        .unwrap();
    match exact_file {
        OpResult::Lookup(LookupResult::Entry(result)) => {
            let entry = &result.target;
            assert_eq!(entry.name, "lazy");
            assert!(matches!(entry.kind, EntryKind::File(_)));
        },
        other => panic!("expected file Lookup, got {other:?}"),
    }
}

#[tokio::test]
async fn test_subtree_handoff() {
    let harness = make_initialized_runtime(
        r#"
        {
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let lookup = harness
        .runtime
        .call_lookup_child("", "checkout")
        .await
        .unwrap();
    assert!(matches!(
        lookup,
        OpResult::Lookup(LookupResult::Subtree(777))
    ));

    let listing = harness
        .runtime
        .call_list_children("checkout")
        .await
        .unwrap();
    assert!(matches!(listing, OpResult::List(ListResult::Subtree(777))));
}

/// Test that lookup sibling files are projected into the cache.
#[tokio::test]
async fn test_lookup_projects_sibling_files_into_cache() {
    let harness = make_initialized_runtime(
        r#"
        {
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let result = harness
        .runtime
        .call_lookup_child("hello", "bundle")
        .await
        .unwrap();

    match &result {
        OpResult::Lookup(LookupResult::Entry(result)) => {
            assert_eq!(result.sibling_files.len(), 2, "expected 2 sibling files");
        },
        other => panic!("expected Lookup, got {other:?}"),
    }

    // Verify the sibling files were projected into the cache.
    let title = harness
        .runtime
        .cache_get("hello/bundle/title", RecordKind::File)
        .expect("title should be in cache");
    let body = harness
        .runtime
        .cache_get("hello/bundle/body", RecordKind::File)
        .expect("body should be in cache");
    let bundle_dirents = harness
        .runtime
        .cache_get("hello/bundle", RecordKind::Dirents)
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
async fn test_lookup_projects_siblings_into_cache() {
    let harness = make_initialized_runtime(
        r#"
        {
            "plugin": "test_provider.wasm",
            "mount": "test",
            "capabilities": {
                "domains": ["httpbin.org"]
            }
        }
    "#,
    );

    let result = harness
        .runtime
        .call_lookup_child("hello", "snapshot")
        .await
        .unwrap();

    match &result {
        OpResult::Lookup(LookupResult::Entry(result)) => {
            let target = &result.target;
            assert_eq!(target.name, "snapshot");

            let mut sibling_names: Vec<_> = result
                .siblings
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            sibling_names.sort_unstable();
            assert_eq!(sibling_names, vec!["comments", "status"]);
        },
        other => panic!("expected Lookup, got {other:?}"),
    }

    let dirents_record = harness
        .runtime
        .cache_get("hello/snapshot", RecordKind::Dirents)
        .expect("snapshot dirents should be cached");

    let dirents = DirentsPayload::deserialize(&dirents_record.payload)
        .expect("dirents payload should deserialize");
    let mut entry_names: Vec<_> = dirents
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    entry_names.sort_unstable();
    // Per the Stage 1b.2c siblings contract (settled at d4e9e98), the
    // host caches children of the target at the target's path, not the
    // target itself. Prior to that fix the target leaked into its own
    // dirents as a self-reference; the test was written against that
    // stale behavior and now asserts the corrected shape.
    assert_eq!(entry_names, vec!["comments", "status"]);
    assert!(dirents.exhaustive);

    let status_lookup = harness
        .runtime
        .cache_get("hello/snapshot/status", RecordKind::Lookup)
        .expect("status lookup should be cached");
    let Some(LookupPayload::Positive(status_meta)) =
        LookupPayload::deserialize(&status_lookup.payload)
    else {
        panic!("expected positive status lookup");
    };
    assert_eq!(status_meta.kind, EntryKindCache::File);
    assert_eq!(status_meta.st_size(), 5);

    let status_attr = harness
        .runtime
        .cache_get("hello/snapshot/status", RecordKind::Attr)
        .expect("status attr should be cached");
    let Some(AttrPayload { meta: status_meta }) = AttrPayload::deserialize(&status_attr.payload)
    else {
        panic!("expected status attr payload");
    };
    assert_eq!(status_meta.kind, EntryKindCache::File);
    assert_eq!(status_meta.st_size(), 5);

    let comments_lookup = harness
        .runtime
        .cache_get("hello/snapshot/comments", RecordKind::Lookup)
        .expect("comments lookup should be cached");
    let Some(LookupPayload::Positive(comments_meta)) =
        LookupPayload::deserialize(&comments_lookup.payload)
    else {
        panic!("expected positive comments lookup");
    };
    assert_eq!(comments_meta.kind, EntryKindCache::Directory);
    assert_eq!(comments_meta.st_size(), 0);
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
        .cache_put("owner/repo", RecordKind::Attr, &record);
    harness
        .runtime
        .cache_put("owner/repo/issues", RecordKind::Attr, &record);
    harness
        .runtime
        .cache_put("owner/repobaz", RecordKind::Attr, &record);

    harness.runtime.cache_delete_prefix("owner/repo");

    assert!(
        harness
            .runtime
            .cache_get("owner/repo", RecordKind::Attr)
            .is_none()
    );
    assert!(
        harness
            .runtime
            .cache_get("owner/repo/issues", RecordKind::Attr)
            .is_none()
    );
    assert!(
        harness
            .runtime
            .cache_get("owner/repobaz", RecordKind::Attr)
            .is_some()
    );
}

#[tokio::test]
async fn test_cache_isolated_by_mount_name() {
    let engine = make_engine();
    let config = InstanceConfig::parse(
        r#"
        {
            "plugin": "test_provider.wasm",
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
    let cloner = Arc::new(GitCloner::new(clone_dir.path().to_path_buf()));
    let extractor = support::make_extractor();
    let runtime_a = CalloutRuntime::new(
        &engine,
        &support::provider_wasm_path(&config.plugin),
        &config,
        cloner.clone(),
        cache_dir.path(),
        "mount-a",
        extractor.clone(),
    )
    .unwrap();
    let runtime_b = CalloutRuntime::new(
        &engine,
        &support::provider_wasm_path(&config.plugin),
        &config,
        cloner,
        cache_dir.path(),
        "mount-b",
        extractor,
    )
    .unwrap();

    runtime_a.initialize().unwrap();
    runtime_b.initialize().unwrap();

    let result = runtime_a.call_list_children("hello").await.unwrap();
    assert!(matches!(result, OpResult::List(ListResult::Entries(_))));
    assert!(runtime_a.cache_get("hello", RecordKind::Dirents).is_some());
    assert!(runtime_b.cache_get("hello", RecordKind::Dirents).is_none());

    let scoped_a = runtime_a.call_list_children("scoped").await.unwrap();
    let scoped_b = runtime_b.call_list_children("scoped").await.unwrap();
    assert!(matches!(scoped_a, OpResult::List(ListResult::Entries(_))));
    assert!(matches!(scoped_b, OpResult::List(ListResult::Entries(_))));
    assert!(
        runtime_a
            .cache_get("scoped/item", RecordKind::Lookup)
            .is_some()
    );
    assert!(
        runtime_b
            .cache_get("scoped/item", RecordKind::Lookup)
            .is_some()
    );

    let tick = runtime_a.call_timer_tick().await.unwrap();
    assert!(matches!(tick, OpResult::Event(_)));
    assert!(
        runtime_a
            .cache_get("scoped/item", RecordKind::Lookup)
            .is_none()
    );
    assert!(
        runtime_b
            .cache_get("scoped/item", RecordKind::Lookup)
            .is_some()
    );
    assert!(
        runtime_a
            .drain_invalidated_paths()
            .into_iter()
            .any(|path| path == "scoped/item")
    );
}
