//! Data-driven oura scenarios over the callout tape system.
//!
//! Fixed historical dates: the provider derives every fetch range from the
//! requested path's `{day}` segment (`Day::preload_range`, `providers/oura/src/
//! lib.rs`), never from wall-clock "now", so a scenario step's date is
//! deterministic regardless of when it is recorded or replayed. `2026-01-15`
//! matches the date the pre-existing hand-written tests already use.
//!
//! `BodyPolicy::RewrittenJson`: oura is the one provider whose upstream
//! responses carry a real account's private biometric data (heart rate,
//! sleep/readiness scores, `HRV`, temperature, `SpO2`) that no test tenant can
//! avoid, so the tape body is parsed, rewritten, and re-serialized instead of
//! stored verbatim (plan section 3.4). `sanitize_fields` and `normalize_fields`
//! below are PRELIMINARY: part 2's actual recording against real Oura API v2
//! payloads is what proves the field-name lists are complete, and the lists
//! land as part of that recording commit for review.
//!
//! `normalize_fields` deliberately excludes the provider's own date-grouping
//! keys (`day`, `timestamp`, `recorded_at`, `start_datetime`, `end_datetime`,
//! `bedtime_start`, `bedtime_end`; see `DATE_FIELDS` in the provider). Those
//! fields are not just display noise: `group_by_day`/`item_day` parse them at
//! REPLAY time (the tape body is fed straight back through the provider's real
//! parse) to bucket each response row into a day. Replacing them with the fixed
//! `<volatile>` string would make `date_value` fail to parse, silently dropping
//! every row from its day bucket and diverging the replayed projection from
//! the recorded one. They are also not a privacy leak in this scenario shape:
//! the requested day is already the path segment, so the body's date fields
//! disclose nothing the path did not already disclose.

use omnifs_itest::scenario::{RecordAuth, Scenario, Step, run};
use omnifs_itest::tape::scrub::{BodyPolicy, TapeRules};

/// The oura mount config the scenarios record against: oura declares only an
/// oauth scheme (`providers/oura/src/lib.rs`), so the auth block selects it
/// explicitly, and the record-mode credential seeding in `scenario.rs` writes
/// an oauth-kind entry (not static-token) because it derives the entry shape
/// from this block.
const OURA_CONFIG: &str = r#"
{
    "provider": "omnifs_provider_oura.wasm",
    "mount": "oura",
    "auth": {
        "type": "oauth",
        "scheme": "oauth"
    },
    "capabilities": {
        "domains": ["api.ouraring.com"]
    }
}
"#;

/// Biometric value fields sanitized to a deterministic, non-reversible token.
/// PRELIMINARY: derived from the public Oura API v2 field names for the
/// collections this scenario file reads (`daily_sleep`, `heart_rate`); part 2
/// checks this against the actual recorded payload and extends it to the
/// fields any other recorded scenario touches.
const SANITIZE_FIELDS: &[&str] = &[
    // Daily summary scores and their sub-score breakdowns.
    "score",
    "contributors",
    // Heart rate.
    "bpm",
    "average_heart_rate",
    "lowest_heart_rate",
    "resting_heart_rate",
    "heart_rate",
    // Heart rate variability.
    "hrv",
    "average_hrv",
    "hrv_balance",
    // Body temperature deviation.
    "temperature_deviation",
    "temperature_trend_deviation",
    "temperature_delta",
    // SpO2 / breathing.
    "spo2_percentage",
    "breathing_disturbance_index",
    "average_breath",
    // Sleep durations and phases (seconds).
    "total_sleep_duration",
    "deep_sleep_duration",
    "light_sleep_duration",
    "rem_sleep_duration",
    "awake_time",
    "latency",
    "efficiency",
    "restless_periods",
    "time_in_bed",
    "movement_30_sec",
    "sleep_phase_5_min",
];

/// Churn fields with no meaning across re-records, safe to normalize because
/// (unlike `DATE_FIELDS`) the provider never parses them. PRELIMINARY: part 2
/// extends this once the real payload shape is visible.
const NORMALIZE_FIELDS: &[&str] = &["id"];

fn oura_rules() -> TapeRules {
    TapeRules {
        drop_response_headers: &[],
        body: BodyPolicy::RewrittenJson {
            sanitize_fields: SANITIZE_FIELDS,
            normalize_fields: NORMALIZE_FIELDS,
        },
    }
}

/// A day file's date-range fetch and neighbor-day storage: reading one day's
/// `daily_sleep.json` issues a single one-month-window fetch
/// (`start_date`/`end_date`, `RangeKind::Date`) and partitions the response
/// into a canonical per day; a follow-up read of a neighboring day inside that
/// window (`2026-01-05`, within the +/-15-day `PRELOAD_RADIUS` of
/// `2026-01-15`) proves the neighbor really was stored: it resolves warm, with
/// no second fetch. Mirrors the existing hand-written
/// `day_file_reads_one_month_date_range_and_stores_neighbor_days` test.
#[test]
#[ignore = "tape pending part-2 recording"]
fn day_file_read() {
    run(&Scenario {
        name: "day-file-read",
        dir: "oura",
        config: OURA_CONFIG,
        auth: Some(RecordAuth {
            token_env: "OMNIFS_RECORD_OURA_TOKEN",
        }),
        rules: oura_rules(),
        setup: None,
        steps: &[
            Step::Read("/2026-01-15/daily_sleep.json"),
            Step::Read("/2026-01-05/daily_sleep.json"),
        ],
    });
}

/// Routing and listing: the open root (only the generated `README.md`), a date
/// directory resolving structurally with no fetch, its exhaustive listing of
/// every collection file, a non-day-indexed name confirmed absent under a day
/// directory (`ring_configuration.json` is not one of the fixed per-day
/// collection files `DayKey::entries` lists), and a time-series day file read
/// (`heart_rate.json`, `RangeKind::DateTime`, `start_datetime`/`end_datetime`
/// query params) grouped by each row's `timestamp` day rather than a `day`
/// field. Mirrors the existing hand-written
/// `root_is_open_and_date_directories_list_day_files`,
/// `non_day_indexed_collections_do_not_resolve_as_day_files`, and
/// `time_series_day_file_reads_datetime_range_and_groups_by_timestamp_day`
/// tests.
#[test]
#[ignore = "tape pending part-2 recording"]
fn listing() {
    run(&Scenario {
        name: "listing",
        dir: "oura",
        config: OURA_CONFIG,
        auth: Some(RecordAuth {
            token_env: "OMNIFS_RECORD_OURA_TOKEN",
        }),
        rules: oura_rules(),
        setup: None,
        steps: &[
            Step::List("/"),
            Step::Lookup {
                parent: "/",
                name: "2026-01-15",
            },
            Step::List("/2026-01-15"),
            Step::Lookup {
                parent: "/2026-01-15",
                name: "ring_configuration.json",
            },
            Step::Read("/2026-01-15/heart_rate.json"),
        ],
    });
}
