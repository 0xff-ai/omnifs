#![cfg(not(target_os = "wasi"))]

mod scenarios;

// None of these tests are adversarial in the established sense (upstream
// error mapping, retry-on-callout-error, malformed bodies, domain denial):
// every one drives a canned happy-path response through provider routing, so
// none is grouped under `mod adversarial`. Each is a candidate for deletion
// once `scenarios.rs` is recorded and its snapshot is confirmed to exercise
// the same op + assertion surface (I7); marked below with the scenario that
// is expected to cover it.

use omnifs_itest::{RuntimeHarness, TestOpExt};
use omnifs_wit::provider::types::{
    ByteSource, CalloutResult, EntryKind, FileSize, FsKind, Header, HttpResponse,
    ListChildrenResult, LookupChildResult, OpResult, ReadFileOutcome, ReadMode, Stability,
};
use serde_json::{Value, json};

const COLLECTION_FILES: &[&str] = &[
    "daily_activity.json",
    "daily_cardiovascular_age.json",
    "daily_readiness.json",
    "daily_resilience.json",
    "daily_sleep.json",
    "daily_spo2.json",
    "daily_stress.json",
    "enhanced_tag.json",
    "heart_rate.json",
    "rest_mode_period.json",
    "ring_battery_level.json",
    "session.json",
    "sleep.json",
    "sleep_time.json",
    "tag.json",
    "vo2_max.json",
    "workout.json",
];

fn oura_harness() -> RuntimeHarness {
    RuntimeHarness::new(
        r#"
        {
            "provider": "omnifs_provider_oura.wasm",
            "mount": "oura",
            "capabilities": {
                "domains": ["api.ouraring.com"]
            }
        }
    "#,
    )
    .unwrap()
}

fn resume_json(op: &mut omnifs_engine::test_support::TestOp<'_>, body: &'static [u8]) {
    op.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: vec![Header {
            name: "etag".to_string(),
            value: "\"v1\"".to_string(),
        }],
        body: body.to_vec(),
    })])
    .unwrap();
}

fn read_query_body(op: &omnifs_engine::test_support::TestOp<'_>) -> Vec<u8> {
    match op.result().unwrap() {
        OpResult::ReadFile(ReadFileOutcome::Found(file)) => {
            assert_eq!(file.attrs.stability, Stability::Dynamic);
            assert_eq!(file.attrs.version_token.as_deref(), Some("\"v1\""));
            match &file.bytes {
                ByteSource::Canonical => op.effects().unwrap().canonical[0].bytes.clone(),
                ByteSource::Inline(bytes) => bytes.clone(),
                other => panic!("expected canonical or inline read, got {other:?}"),
            }
        },
        other => panic!("expected found read, got {other:?}"),
    }
}

fn read_json(op: &omnifs_engine::test_support::TestOp<'_>) -> Value {
    serde_json::from_slice(&read_query_body(op)).unwrap()
}

fn canonical_paths(op: &omnifs_engine::test_support::TestOp<'_>) -> Vec<String> {
    let effects = op.effects().unwrap();
    effects
        .canonical
        .iter()
        .flat_map(|store| store.view_leaves.iter().cloned())
        .collect()
}

fn canonical_json(op: &omnifs_engine::test_support::TestOp<'_>, path: &str) -> Value {
    let effects = op.effects().unwrap();
    let store = effects
        .canonical
        .iter()
        .find(|store| store.view_leaves.iter().any(|leaf| leaf == path))
        .unwrap_or_else(|| panic!("expected canonical store for {path}"));
    serde_json::from_slice(&store.bytes).unwrap()
}

fn assert_projected_dir(op: &omnifs_engine::test_support::TestOp<'_>, path: &str) {
    let write = op
        .effects()
        .unwrap()
        .fs
        .iter()
        .find(|write| write.path == path)
        .unwrap_or_else(|| panic!("expected projected directory {path}"));
    assert!(
        write.id.is_none(),
        "directory should not carry an object id"
    );
    assert!(
        matches!(write.kind, FsKind::Directory(false)),
        "expected non-exhaustive directory projection for {path}, got {:?}",
        write.kind
    );
}

fn assert_projected_deferred_file_with_exact_size(
    op: &omnifs_engine::test_support::TestOp<'_>,
    path: &str,
) {
    let write = op
        .effects()
        .unwrap()
        .fs
        .iter()
        .find(|write| write.path == path)
        .unwrap_or_else(|| panic!("expected projected lazy file {path}"));
    assert!(write.id.is_some(), "file should carry its object id");
    let FsKind::File(file) = &write.kind else {
        panic!("expected file projection for {path}, got {:?}", write.kind);
    };
    assert_eq!(file.attrs.stability, Stability::Dynamic);
    assert_eq!(file.attrs.version_token.as_deref(), Some("\"v1\""));
    assert!(matches!(file.attrs.size, FileSize::Exact(_)));
    assert!(matches!(file.bytes, ByteSource::Deferred(ReadMode::Full)));
}

// TODO(tape): covered by scenarios::listing?
#[test]
fn root_is_open_and_date_directories_list_day_files() {
    let harness = oura_harness();

    let root = harness.list("/").unwrap();
    match root.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            assert!(!listing.exhaustive);
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["README.md"]);
        },
        other => panic!("expected root listing, got {other:?}"),
    }

    let day_lookup = harness.lookup("/", "2026-01-15").unwrap();
    match day_lookup.result().unwrap() {
        OpResult::LookupChild(LookupChildResult::Entry(result)) => {
            assert_eq!(result.target.name, "2026-01-15");
            assert!(matches!(result.target.kind, EntryKind::Directory));
        },
        other => panic!("expected date directory lookup, got {other:?}"),
    }

    let day = harness.list("/2026-01-15").unwrap();
    match day.result().unwrap() {
        OpResult::ListChildren(ListChildrenResult::Entries(listing)) => {
            assert!(listing.exhaustive);
            let mut names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort_unstable();
            assert_eq!(names, COLLECTION_FILES);
        },
        other => panic!("expected day listing, got {other:?}"),
    }
}

// TODO(tape): covered by scenarios::listing?
#[test]
fn non_day_indexed_collections_do_not_resolve_as_day_files() {
    let harness = oura_harness();

    let lookup = harness
        .lookup("/2026-01-15", "ring_configuration.json")
        .unwrap();
    match lookup.result().unwrap() {
        OpResult::LookupChild(LookupChildResult::NotFound(_)) => {},
        other => panic!("expected ring_configuration to be absent, got {other:?}"),
    }
}

// TODO(tape): covered by scenarios::day_file_read?
#[test]
fn day_file_reads_one_month_date_range_and_stores_neighbor_days() {
    let harness = oura_harness();
    let mut op = harness.read("/2026-01-15/daily_sleep.json").unwrap();
    let fetch = op.expect_single_fetch();
    assert_eq!(
        fetch.url,
        "https://api.ouraring.com/v2/usercollection/daily_sleep?start_date=2025-12-31&end_date=2026-01-30"
    );

    resume_json(
        &mut op,
        br#"{"data":[{"id":"sleep-before","day":"2026-01-14"},{"id":"sleep-current","day":"2026-01-15"},{"id":"sleep-after","day":"2026-01-30"}]}"#,
    );
    assert_eq!(
        read_json(&op),
        json!({"data":[{"day":"2026-01-15","id":"sleep-current"}]})
    );

    let paths = canonical_paths(&op);
    assert_eq!(paths.len(), 31);
    assert!(paths.iter().all(|path| path.ends_with("/daily_sleep.json")));
    assert!(
        paths
            .iter()
            .any(|path| path == "/2026-01-14/daily_sleep.json")
    );
    assert!(
        paths
            .iter()
            .any(|path| path == "/2026-01-15/daily_sleep.json")
    );
    assert!(
        paths
            .iter()
            .any(|path| path == "/2026-01-30/daily_sleep.json")
    );
    assert_eq!(
        canonical_json(&op, "/2026-01-14/daily_sleep.json"),
        json!({"data":[{"day":"2026-01-14","id":"sleep-before"}]})
    );

    assert_projected_dir(&op, "/2026-01-14");
    assert_projected_deferred_file_with_exact_size(&op, "/2026-01-14/daily_sleep.json");
    assert_projected_dir(&op, "/2026-01-15");
    assert_projected_deferred_file_with_exact_size(&op, "/2026-01-15/daily_sleep.json");
    assert_projected_dir(&op, "/2026-01-30");
    assert_projected_deferred_file_with_exact_size(&op, "/2026-01-30/daily_sleep.json");
    assert_eq!(
        op.effects()
            .unwrap()
            .fs
            .iter()
            .filter(|write| matches!(write.kind, FsKind::Directory(_)))
            .count(),
        31
    );
    assert_eq!(
        op.effects()
            .unwrap()
            .fs
            .iter()
            .filter(|write| matches!(write.kind, FsKind::File(_)))
            .count(),
        31
    );
}

// TODO(tape): covered by scenarios::listing?
#[test]
fn time_series_day_file_reads_datetime_range_and_groups_by_timestamp_day() {
    let harness = oura_harness();
    let mut op = harness.read("/2026-01-15/heart_rate.json").unwrap();
    let fetch = op.expect_single_fetch();
    assert_eq!(
        fetch.url,
        "https://api.ouraring.com/v2/usercollection/heartrate?start_datetime=2025-12-31T00%3A00%3A00Z&end_datetime=2026-01-30T23%3A59%3A59Z"
    );

    resume_json(
        &mut op,
        br#"{"data":[{"bpm":60,"timestamp":"2026-01-15T10:00:00Z"},{"bpm":62,"timestamp":"2026-01-16T10:00:00Z"}]}"#,
    );
    assert_eq!(
        read_json(&op),
        json!({"data":[{"bpm":60,"timestamp":"2026-01-15T10:00:00Z"}]})
    );
    assert_eq!(
        canonical_json(&op, "/2026-01-16/heart_rate.json"),
        json!({"data":[{"bpm":62,"timestamp":"2026-01-16T10:00:00Z"}]})
    );
}
