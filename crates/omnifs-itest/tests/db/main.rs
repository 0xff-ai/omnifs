#![cfg(not(target_os = "wasi"))]

mod db_fixture;

use omnifs_core::path::Path;
use omnifs_host::{Error, LookupOutcome};
use omnifs_itest::{RuntimeHarness, make_runtime_from_config};
use omnifs_wit::provider::types::{ByteSource, EntryKind, ErrorKind, ListChildrenResult};
use serde::Deserialize;

fn parse_path(s: &str) -> Path {
    Path::parse(s).unwrap()
}

fn assert_lookup_not_found(lookup: &LookupOutcome) {
    assert!(
        matches!(lookup, LookupOutcome::NotFound),
        "expected lookup miss, got {lookup:?}"
    );
}

fn db_config(host: &str) -> String {
    format!(
        r#"{{
            "provider": "omnifs_provider_db.wasm",
            "mount": "db",
            "capabilities": {{
                "preopened_paths": [{{ "host": "{host}", "guest": "/data", "mode": "ro" }}]
            }},
            "config": {{
                "database_type": "sqlite",
                "path": "/data/chinook.sqlite",
                "read_only": true,
                "sample_limit": 20
            }}
        }}"#
    )
}

fn db_harness() -> (tempfile::TempDir, RuntimeHarness) {
    let dir = tempfile::tempdir().unwrap();
    db_fixture::write_chinook_fixture(dir.path());
    let harness = make_runtime_from_config(&db_config(&dir.path().display().to_string()));
    (dir, harness)
}

async fn read_bytes(harness: &RuntimeHarness, path: &str) -> Vec<u8> {
    let result = harness
        .runtime
        .namespace()
        .read_file(
            &parse_path(path),
            Path::parse(path)
                .unwrap()
                .content_type_mime(None)
                .to_string(),
            None,
        )
        .await
        .unwrap();
    match &result.bytes {
        ByteSource::Inline(bytes) => bytes.clone(),
        ByteSource::Canonical => panic!("db provider must not use canonical cache for {path}"),
        other => panic!("expected inline file content for {path}, got {other:?}"),
    }
}

#[derive(Debug, Deserialize)]
struct TableDoc {
    #[allow(dead_code)]
    name: String,
    create_sql: Option<String>,
    columns: serde_json::Value,
    indexes: serde_json::Value,
    row_count: i64,
}

#[derive(Debug, Deserialize)]
struct FileInfo {
    #[allow(dead_code)]
    path: String,
    sqlite_version: String,
}

#[tokio::test]
async fn db_tables_listing_exhaustive_names() {
    let (_dir, harness) = db_harness();

    let root = harness
        .runtime
        .namespace()
        .list_children(&parse_path("/"), None, None, None)
        .await
        .unwrap();
    match root {
        ListChildrenResult::Entries(listing) => {
            let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&"meta"));
            assert!(names.contains(&"tables"));
            assert_eq!(names.len(), 2);
        },
        other => panic!("expected root listing, got {other:?}"),
    }

    let tables = harness
        .runtime
        .namespace()
        .list_children(&parse_path("/tables"), None, None, None)
        .await
        .unwrap();
    match tables {
        ListChildrenResult::Entries(listing) => {
            assert!(listing.exhaustive);
            let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&"Album"));
            assert!(names.contains(&"Artist"));
            assert!(names.contains(&"Wide"));
            assert!(
                listing
                    .entries
                    .iter()
                    .all(|e| matches!(e.kind, EntryKind::Directory))
            );
        },
        other => panic!("expected tables listing, got {other:?}"),
    }

    let missing = harness
        .runtime
        .namespace()
        .lookup_child(&parse_path("/tables"), "NoSuchTable", None)
        .await
        .unwrap();
    assert_lookup_not_found(&missing);
}

#[tokio::test]
async fn db_meta_listing_is_direct_path_surface() {
    let (_dir, harness) = db_harness();

    let meta = harness
        .runtime
        .namespace()
        .list_children(&parse_path("/meta"), None, None, None)
        .await
        .unwrap();

    match meta {
        ListChildrenResult::Entries(listing) => {
            assert!(listing.exhaustive);
            let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&"info.json"));
            assert!(names.contains(&"version.txt"));
            assert!(names.contains(&"path.txt"));
        },
        other => panic!("expected meta listing, got {other:?}"),
    }
    assert!(
        harness
            .runtime
            .cached_canonical_for(&parse_path("/meta/info.json"))
            .is_none()
    );
    assert!(
        harness
            .runtime
            .cached_canonical_for(&parse_path("/meta/version.txt"))
            .is_none()
    );
}

#[tokio::test]
async fn db_table_direct_files_are_coherent() {
    let (_dir, harness) = db_harness();

    let table_bytes = read_bytes(&harness, "/tables/Album/table.json").await;
    let doc: TableDoc = serde_json::from_slice(&table_bytes).unwrap();

    let schema_sql = read_bytes(&harness, "/tables/Album/schema.sql").await;
    let schema_json = read_bytes(&harness, "/tables/Album/schema.json").await;
    let indexes_json = read_bytes(&harness, "/tables/Album/indexes.json").await;
    let count_txt = read_bytes(&harness, "/tables/Album/count.txt").await;

    assert_eq!(
        doc.create_sql.as_deref().unwrap_or("").as_bytes(),
        schema_sql.as_slice()
    );

    let expected_schema_json = {
        let mut bytes = serde_json::to_vec_pretty(&doc.columns).unwrap();
        bytes.push(b'\n');
        bytes
    };
    assert_eq!(expected_schema_json, schema_json);

    let expected_indexes_json = {
        let mut bytes = serde_json::to_vec_pretty(&doc.indexes).unwrap();
        bytes.push(b'\n');
        bytes
    };
    assert_eq!(expected_indexes_json, indexes_json);

    assert_eq!(
        doc.row_count.to_string(),
        String::from_utf8_lossy(&count_txt).trim()
    );
    assert_eq!(doc.row_count, 3);

    let schema_text = String::from_utf8_lossy(&schema_sql);
    assert!(schema_text.contains("CREATE TABLE Album"));
    assert_eq!(count_txt, b"3\n");
}

#[tokio::test]
async fn db_table_direct_files_do_not_use_canonical_cache() {
    let (_dir, harness) = db_harness();

    let table_bytes = read_bytes(&harness, "/tables/Album/table.json").await;

    let schema_sql = read_bytes(&harness, "/tables/Album/schema.sql").await;
    let schema_json = read_bytes(&harness, "/tables/Album/schema.json").await;
    let indexes_json = read_bytes(&harness, "/tables/Album/indexes.json").await;
    let count_txt = read_bytes(&harness, "/tables/Album/count.txt").await;

    let doc: TableDoc = serde_json::from_slice(&table_bytes).unwrap();
    assert_eq!(
        doc.create_sql.as_deref().unwrap_or("").as_bytes(),
        schema_sql.as_slice()
    );
    let mut expected_schema_json = serde_json::to_vec_pretty(&doc.columns).unwrap();
    expected_schema_json.push(b'\n');
    assert_eq!(expected_schema_json, schema_json);
    let mut expected_indexes_json = serde_json::to_vec_pretty(&doc.indexes).unwrap();
    expected_indexes_json.push(b'\n');
    assert_eq!(expected_indexes_json, indexes_json);
    assert_eq!(count_txt, b"3\n");

    assert!(
        harness
            .runtime
            .cached_canonical_for(&parse_path("/tables/Album/table.json"))
            .is_none()
    );
    assert!(
        harness
            .runtime
            .cached_canonical_for(&parse_path("/tables/Album/schema.json"))
            .is_none()
    );
}

#[tokio::test]
async fn db_missing_table_negative_record() {
    let (_dir, harness) = db_harness();

    let lookup = harness
        .runtime
        .namespace()
        .lookup_child(&parse_path("/tables"), "NoSuchTable", None)
        .await
        .unwrap();
    assert_lookup_not_found(&lookup);

    let read_err = harness
        .runtime
        .namespace()
        .read_file(
            &parse_path("/tables/NoSuchTable/schema.sql"),
            Path::parse("/tables/NoSuchTable/schema.sql")
                .unwrap()
                .content_type_mime(None)
                .to_string(),
            None,
        )
        .await
        .unwrap_err();
    match read_err {
        Error::ProviderError(error) => assert_eq!(error.kind, ErrorKind::NotFound),
        other => panic!("expected NotFound read, got {other:?}"),
    }

    let lookup_again = harness
        .runtime
        .namespace()
        .lookup_child(&parse_path("/tables"), "NoSuchTable", None)
        .await
        .unwrap();
    assert_lookup_not_found(&lookup_again);
}

#[tokio::test]
async fn db_sample_served_ranged() {
    let (_dir, harness) = db_harness();

    // `sample.json` is declared `ranged`, so every sample (any size) is served
    // through the open-file/read-chunk session, never a full read.
    let wide = read_ranged(&harness, "/tables/Wide/sample.json").await;
    assert!(wide.starts_with(b"["));

    let album = read_ranged(&harness, "/tables/Album/sample.json").await;
    let album_text = String::from_utf8_lossy(&album);
    assert!(album_text.contains("For Those About To Rock"));
}

/// Read a ranged file whole: open, pull chunks until EOF, close.
async fn read_ranged(harness: &RuntimeHarness, path: &str) -> Vec<u8> {
    let ns = harness.runtime.namespace();
    let opened = ns.open_file(&parse_path(path)).await.unwrap();
    let mut bytes = Vec::new();
    loop {
        let chunk = ns
            .read_chunk(opened.handle, bytes.len() as u64, 64 * 1024)
            .await
            .unwrap();
        bytes.extend_from_slice(&chunk.content);
        if chunk.eof {
            break;
        }
    }
    harness.runtime.call_close_file(opened.handle).unwrap();
    bytes
}

#[tokio::test]
async fn db_meta_direct_files() {
    let (_dir, harness) = db_harness();

    let info_bytes = read_bytes(&harness, "/meta/info.json").await;
    let info: FileInfo = serde_json::from_slice(&info_bytes).unwrap();

    let version = read_bytes(&harness, "/meta/version.txt").await;
    let path = read_bytes(&harness, "/meta/path.txt").await;

    assert_eq!(version, format!("{}\n", info.sqlite_version).into_bytes());
    assert_eq!(path, b"/data/chinook.sqlite\n");
}

#[tokio::test]
async fn db_readonly_immutable_no_revalidation() {
    let (_dir, harness) = db_harness();

    let first = read_bytes(&harness, "/tables/Album/count.txt").await;
    let second = read_bytes(&harness, "/tables/Album/count.txt").await;
    assert_eq!(first, second);
    assert_eq!(first, b"3\n");
}
