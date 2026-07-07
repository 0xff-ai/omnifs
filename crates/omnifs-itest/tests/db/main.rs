#![cfg(not(target_os = "wasi"))]

mod db_fixture;

use omnifs_core::path::Path;
use omnifs_engine::EngineError;
use omnifs_engine::test_support::{LookupOutcome, NamespaceListOutcome, ReadBytes};
use omnifs_itest::RuntimeHarness;
use omnifs_wit::provider::types::ErrorKind;
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

fn db_config(host_file: &str) -> String {
    format!(
        r#"{{
            "provider": "omnifs_provider_db.wasm",
            "mount": "db",
            "capabilities": {{
                "preopened_paths": {{ "dynamic": true }}
            }},
            "config": {{
                "path": "{host_file}",
                "read_only": true,
                "sample_limit": 20
            }}
        }}"#
    )
}

fn db_harness() -> (tempfile::TempDir, RuntimeHarness) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = db_fixture::write_chinook_fixture(dir.path());
    let harness = RuntimeHarness::new(&db_config(&db_path.display().to_string())).unwrap();
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
        ReadBytes::Inline(bytes) => bytes.clone(),
        ReadBytes::Canonical => panic!("db provider must not use canonical cache for {path}"),
        other @ ReadBytes::Blob(_) => {
            panic!("expected inline file content for {path}, got {other:?}")
        },
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
        NamespaceListOutcome::Entries(listing) => {
            let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&"README.md"));
            assert!(names.contains(&"meta"));
            assert!(names.contains(&"tables"));
            assert_eq!(names.len(), 3);
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
        NamespaceListOutcome::Entries(listing) => {
            assert!(listing.exhaustive);
            let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&"README.md"));
            assert!(names.contains(&"Album"));
            assert!(names.contains(&"Artist"));
            assert!(names.contains(&"Wide"));
            assert!(
                listing
                    .entries
                    .iter()
                    .filter(|entry| entry.name != "README.md")
                    .all(|entry| entry.meta.is_directory())
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
        NamespaceListOutcome::Entries(listing) => {
            assert!(listing.exhaustive);
            let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&"README.md"));
            assert!(names.contains(&"info.json"));
            assert!(names.contains(&"version.txt"));
            assert!(names.contains(&"path.txt"));
        },
        other => panic!("expected meta listing, got {other:?}"),
    }
    assert!(harness.cached_canonical_for("/meta/info.json").is_none());
    assert!(harness.cached_canonical_for("/meta/version.txt").is_none());
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
            .cached_canonical_for("/tables/Album/table.json")
            .is_none()
    );
    assert!(
        harness
            .cached_canonical_for("/tables/Album/schema.json")
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
        EngineError::ProviderError(error) => assert_eq!(error.kind, ErrorKind::NotFound),
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
async fn db_sample_served_whole_uncapped() {
    let (_dir, harness) = db_harness();

    // `sample.json` is a fully-materialized body projection, served whole
    // through the read-file terminal with no inline-size cap. The Wide sample
    // (20 rows * ~4 KiB at LIMIT 20) exceeds 64 KiB, proving it is not capped.
    let wide = read_bytes(&harness, "/tables/Wide/sample.json").await;
    assert!(wide.starts_with(b"["));
    assert!(
        wide.len() > 64 * 1024,
        "wide sample should be served whole and uncapped, got {} bytes",
        wide.len()
    );

    let album = read_bytes(&harness, "/tables/Album/sample.json").await;
    let album_text = String::from_utf8_lossy(&album);
    assert!(album_text.contains("For Those About To Rock"));
}

#[tokio::test]
async fn db_meta_direct_files() {
    let (dir, harness) = db_harness();

    let info_bytes = read_bytes(&harness, "/meta/info.json").await;
    let info: FileInfo = serde_json::from_slice(&info_bytes).unwrap();

    let version = read_bytes(&harness, "/meta/version.txt").await;
    let path = read_bytes(&harness, "/meta/path.txt").await;

    assert_eq!(version, format!("{}\n", info.sqlite_version).into_bytes());
    // path.txt echoes the configured path verbatim: under guest == host the
    // provider opens (and reports) the real host file the preopen resolved from.
    let expected = format!("{}\n", dir.path().join("chinook.sqlite").display());
    assert_eq!(path, expected.into_bytes());
}
