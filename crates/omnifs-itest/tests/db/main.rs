#![cfg(not(target_os = "wasi"))]

mod db_fixture;

use omnifs_core::path::Path;
use omnifs_engine::{DirCursor, LookupAnswer, Namespace};
use omnifs_itest::{
    ReadFileOpExt, RuntimeHarness, TestOpExt, expect_inline, make_initialized_runtime, parse_path,
};
use omnifs_wit::provider::types::{EntryKind, ListChildrenResult, LookupChildResult};
use serde::Deserialize;

fn db_config(host_file: &str) -> String {
    format!(
        r#"{{
            "provider": "omnifs_provider_db.wasm",
            "mount": "db",
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
    let harness = make_initialized_runtime(&db_config(&db_path.display().to_string()));
    (dir, harness)
}

fn read_bytes(harness: &RuntimeHarness, path: &str) -> Vec<u8> {
    let result = harness.read(path).unwrap().into_read_file().unwrap();
    expect_inline(&result).to_vec()
}

async fn resolve_namespace(namespace: &dyn Namespace, path: &str) -> LookupAnswer {
    let attrs = namespace.getattr(Path::root()).await.unwrap();
    let mut answer = LookupAnswer::found(Path::root(), attrs);
    for segment in parse_path(path).segments() {
        answer = namespace.lookup(answer.path, segment).await.unwrap();
    }
    answer
}

async fn namespace_read_bytes(harness: &RuntimeHarness, path: &str) -> Vec<u8> {
    let answer = resolve_namespace(harness.namespace.as_ref(), path).await;
    harness
        .namespace
        .read(answer.path, 0, u32::MAX)
        .await
        .unwrap()
        .bytes
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

    let root = harness.list("/").unwrap().into_ok().unwrap();
    match root {
        ListChildrenResult::Entries(listing) => {
            let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert!(names.contains(&"README.md"));
            assert!(names.contains(&"meta"));
            assert!(names.contains(&"tables"));
            assert_eq!(names.len(), 3);
        },
        other => panic!("expected root listing, got {other:?}"),
    }

    let tables = harness.list("/tables").unwrap().into_ok().unwrap();
    match tables {
        ListChildrenResult::Entries(listing) => {
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
                    .all(|entry| matches!(entry.kind, EntryKind::Directory))
            );
        },
        other => panic!("expected tables listing, got {other:?}"),
    }

    let missing = harness
        .lookup("/tables", "NoSuchTable")
        .unwrap()
        .into_ok()
        .unwrap();
    assert!(matches!(missing, LookupChildResult::NotFound(_)));
}

#[tokio::test]
async fn db_meta_listing_is_direct_path_surface() {
    let (_dir, harness) = db_harness();

    let meta_node = resolve_namespace(harness.namespace.as_ref(), "/db/meta").await;
    let meta = harness
        .namespace
        .readdir(meta_node.path, DirCursor::start(), 0)
        .await
        .unwrap();

    assert!(meta.next.is_none());
    let names: Vec<_> = meta
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert!(names.contains(&"README.md"));
    assert!(names.contains(&"info.json"));
    assert!(names.contains(&"version.txt"));
    assert!(names.contains(&"path.txt"));
}

#[tokio::test]
async fn db_table_direct_files_are_coherent() {
    let (_dir, harness) = db_harness();

    let table_bytes = read_bytes(&harness, "/tables/Album/table.json");
    let doc: TableDoc = serde_json::from_slice(&table_bytes).unwrap();

    let schema_sql = read_bytes(&harness, "/tables/Album/schema.sql");
    let schema_json = read_bytes(&harness, "/tables/Album/schema.json");
    let indexes_json = read_bytes(&harness, "/tables/Album/indexes.json");
    let count_txt = read_bytes(&harness, "/tables/Album/count.txt");

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
async fn db_table_direct_files_match_their_database_values() {
    let (_dir, harness) = db_harness();

    let table_bytes = namespace_read_bytes(&harness, "/db/tables/Album/table.json").await;

    let schema_sql = namespace_read_bytes(&harness, "/db/tables/Album/schema.sql").await;
    let schema_json = namespace_read_bytes(&harness, "/db/tables/Album/schema.json").await;
    let indexes_json = namespace_read_bytes(&harness, "/db/tables/Album/indexes.json").await;
    let count_txt = namespace_read_bytes(&harness, "/db/tables/Album/count.txt").await;

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
}

#[tokio::test]
async fn db_missing_table_is_not_found() {
    let (_dir, harness) = db_harness();

    let namespace = harness.namespace.as_ref();
    let tables = resolve_namespace(namespace, "/db/tables").await;
    assert!(matches!(
        namespace.lookup(tables.path.clone(), "NoSuchTable").await,
        Ok(answer) if answer.is_missing()
    ));

    let schema = harness
        .read("/tables/NoSuchTable/schema.sql")
        .unwrap()
        .into_result()
        .unwrap()
        .unwrap_err();
    assert_eq!(
        schema.kind,
        omnifs_wit::provider::types::ErrorKind::NotFound
    );
    assert!(matches!(
        namespace.lookup(tables.path, "NoSuchTable").await,
        Ok(answer) if answer.is_missing()
    ));
}

#[tokio::test]
async fn db_sample_served_whole_uncapped() {
    let (_dir, harness) = db_harness();

    // `sample.json` is a fully-materialized body projection, served whole
    // through the read-file terminal with no inline-size cap. The Wide sample
    // (20 rows * ~4 KiB at LIMIT 20) exceeds 64 KiB, proving it is not capped.
    let wide = read_bytes(&harness, "/tables/Wide/sample.json");
    assert!(wide.starts_with(b"["));
    assert!(
        wide.len() > 64 * 1024,
        "wide sample should be served whole and uncapped, got {} bytes",
        wide.len()
    );

    let album = read_bytes(&harness, "/tables/Album/sample.json");
    let album_text = String::from_utf8_lossy(&album);
    assert!(album_text.contains("For Those About To Rock"));
}

#[tokio::test]
async fn db_meta_direct_files() {
    let (dir, harness) = db_harness();

    let info_bytes = read_bytes(&harness, "/meta/info.json");
    let info: FileInfo = serde_json::from_slice(&info_bytes).unwrap();

    let version = read_bytes(&harness, "/meta/version.txt");
    let path = read_bytes(&harness, "/meta/path.txt");

    assert_eq!(version, format!("{}\n", info.sqlite_version).into_bytes());
    // path.txt echoes the configured path verbatim: under guest == host the
    // provider opens (and reports) the real host file the preopen resolved from.
    let expected = format!("{}\n", dir.path().join("chinook.sqlite").display());
    assert_eq!(path, expected.into_bytes());
}
