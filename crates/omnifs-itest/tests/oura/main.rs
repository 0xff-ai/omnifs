#![cfg(not(target_os = "wasi"))]

mod scenarios;

// Neither remaining test is adversarial in the established sense (upstream
// error mapping, retry-on-callout-error, malformed bodies, domain denial), so
// no `mod adversarial` grouping applies: each survives because it asserts a
// surface the recorded scenarios demonstrably cannot render, stated on the
// test. The other two hand-written tests
// (`non_day_indexed_collections_do_not_resolve_as_day_files`,
// `day_file_reads_one_month_date_range_and_stores_neighbor_days`) were deleted
// once `scenarios.rs`'s recorded snapshots covered their whole op + assertion
// surface (I7): the `listing` scenario renders the identical not-found lookup,
// and `day-file-read`'s step-0 snapshot renders the requested-day body, all 31
// per-day canonicals, and every fs projection (non-exhaustive dirs,
// deferred-full files with exact sizes) while the tape match key enforces the
// range-fetch URL.

use omnifs_itest::{RuntimeHarness, TestOpExt};
use omnifs_wit::provider::types::{
    ByteSource, CalloutResult, EntryKind, Header, HttpResponse, ListChildrenResult,
    LookupChildResult, OpResult, ReadFileOutcome, Stability,
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

fn canonical_json(op: &omnifs_engine::test_support::TestOp<'_>, path: &str) -> Value {
    let effects = op.effects().unwrap();
    let store = effects
        .canonical
        .iter()
        .find(|store| store.view_leaves.iter().any(|leaf| leaf == path))
        .unwrap_or_else(|| panic!("expected canonical store for {path}"));
    serde_json::from_slice(&store.bytes).unwrap()
}

// Kept alongside the `listing` scenario because the step trace renders listing
// ENTRIES but not the listing's exhaustiveness flag: the root must stay
// non-exhaustive (any date directory exists beyond the listed `README.md`)
// while a day directory's collection listing is exhaustive, and only these
// assertions pin that distinction.
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

// Kept alongside the `listing` scenario because a recording can never exercise
// requested-day timestamp grouping: oura paginates `heartrate` at 1000 samples
// chronologically and the provider reads only page one of its 31-day window,
// so the requested day (always mid-window by `PRELOAD_RADIUS` construction)
// comes back empty in every real recording. The synthetic two-row body is the
// only way to prove rows whose `timestamp` falls on the requested day land in
// its bucket rather than a neighbor's.
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
