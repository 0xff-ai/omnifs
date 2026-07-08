//! Data-driven oura scenarios over the callout tape system.
//!
//! Fixed historical dates: the provider derives every fetch range from the
//! requested path's `{day}` segment (`Day::preload_range`, `providers/oura/src/
//! lib.rs`), never from wall-clock "now", so a scenario step's date is
//! deterministic regardless of when it is recorded or replayed. `2026-01-15`
//! matches the date the pre-existing hand-written tests already use, and the
//! recording account (the maintainer's own ring) has data for every day of the
//! derived window (2025-12-31 through 2026-01-30, single page, no pagination
//! cursor on `daily_sleep`).
//!
//! `BodyPolicy::RewrittenJson`: oura is the one provider whose upstream
//! responses carry a real account's private biometric data (heart rate and
//! sleep scores here; every other collection is equally personal) that no test
//! tenant can avoid, so the tape body is parsed, rewritten, and re-serialized
//! instead of stored verbatim (plan section 3.4). `sanitize_fields` and
//! `normalize_fields` below are finalized against the real recorded payloads:
//! together they name every field the two recorded collections return that is
//! not kept-verbatim by name below, scoped to those collections (a future
//! scenario recording another collection extends them against its payload).
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

/// Biometric value fields sanitized to a deterministic, non-reversible token,
/// matched at any JSON nesting depth. Finalized against the real Oura API v2
/// payloads the two recorded collections return:
/// - `score` (`daily_sleep`): the daily sleep score.
/// - `contributors` (`daily_sleep`): the whole sub-score object
///   (`deep_sleep`, `efficiency`, `latency`, `rem_sleep`, `restfulness`,
///   `timing`, `total_sleep`) redacted wholesale to one token; a matched
///   field's value is replaced before descent, so the object's inner values
///   never reach the tape.
/// - `bpm` (`heartrate`): the heart-rate measurement itself.
/// - `source` (`heartrate`): the `awake`/`rest`/`workout`/... state label; a
///   sleep/wake/exercise timeline is body data even with `bpm` redacted.
const SANITIZE_FIELDS: &[&str] = &["score", "contributors", "bpm", "source"];

/// Churn/opaque fields normalized to the fixed `<volatile>` string, safe
/// because (unlike `DATE_FIELDS`) the provider never parses them:
/// - `id` (`daily_sleep` rows): oura's opaque per-document id; carries no
///   projection meaning and could encode account-side state.
/// - `next_token` (response envelope): the pagination cursor, an opaque value
///   the provider never follows (`RangeRequest::fetch` issues exactly one
///   GET); non-null on the `heartrate` window response.
/// - `producer_timestamp` (`heartrate` rows): epoch-millis device batch-upload
///   metadata (identical across consecutive samples), disclosing the user's
///   app-sync times for no projection meaning. Normalizing it also leaves the
///   rewritten bodies with zero raw JSON numbers, so a leak scan for leftover
///   numeric values stays trivially clean.
const NORMALIZE_FIELDS: &[&str] = &["id", "next_token", "producer_timestamp"];

fn oura_rules() -> TapeRules {
    TapeRules {
        // Oura serves through CloudFront and stamps per-request trace ids the
        // base drop list (github-style names) does not cover; all four churn
        // on every request and would diff each re-record.
        drop_response_headers: &["x-trace-id", "x-amz-cf-id", "x-amz-cf-pop", "x-cache"],
        body: BodyPolicy::RewrittenJson {
            sanitize_fields: SANITIZE_FIELDS,
            normalize_fields: NORMALIZE_FIELDS,
        },
    }
}

/// A day file's date-range fetch and neighbor-day storage: reading one day's
/// `daily_sleep.json` issues a single one-month-window fetch
/// (`start_date`/`end_date`, `RangeKind::Date`) and partitions the response
/// into one canonical per window day (31 canonical stores in the step's
/// effects, the neighbor-day storage the hand-written
/// `day_file_reads_one_month_date_range_and_stores_neighbor_days` test
/// asserted). The follow-up read of a neighboring day (`2026-01-05`, inside
/// the +/-15-day `PRELOAD_RADIUS` of `2026-01-15`) records what the real
/// engine does next: oura sends no `etag`, so the stored canonical carries no
/// validator and the dynamic object cannot be served warm; the second read
/// issues its own range fetch centered on its day (the tape carries two
/// entries with shifted windows). A re-record where the second entry
/// disappears means either oura started sending validators or the engine's
/// freshness policy changed.
#[test]
fn day_file_read() {
    run(&Scenario {
        name: "day-file-read",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
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
/// field.
///
/// The recorded reality of the heart-rate read, which the hand-written
/// `time_series_day_file_reads_datetime_range_and_groups_by_timestamp_day`
/// test's two-row synthetic body masked: oura paginates `heartrate` at 1000
/// samples per page, chronologically, and the provider issues exactly one GET
/// for the whole 31-day window without following `next_token`, so page one
/// covers only the window's first day(s) and the requested day's bucket comes
/// back EMPTY (`{"data":[]}`), with the early window days landing as neighbor
/// canonicals. For a dense-HR account the requested day (always mid-window by
/// `PRELOAD_RADIUS` construction) can never be on page one, so this is the
/// provider's real steady-state behavior, snapshot included deliberately: a
/// re-record that suddenly shows data for the requested day means either
/// upstream pagination changed or the provider learned to follow cursors, and
/// both deserve a loud diff. Timestamp-day grouping itself is still proven by
/// the populated neighbor-day canonicals in the step's effects.
#[test]
fn listing() {
    run(&Scenario {
        name: "listing",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
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
