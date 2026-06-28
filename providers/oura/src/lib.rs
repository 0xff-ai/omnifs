#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#![allow(clippy::needless_pass_by_value)]

use core::fmt;
use core::str::FromStr;
use std::ops::RangeInclusive;

use omnifs_sdk::hashbrown::HashMap;
use omnifs_sdk::prelude::*;
use serde_json::Value;
use strum::{EnumProperty, VariantArray};
use time::format_description::well_known::{Iso8601, Rfc3339};
use time::{Date, Duration, Time};

use omnifs_sdk::auth::{Auth, OAuth, Scheme};

const PRELOAD_RADIUS: Duration = Duration::days(15);
const JSON_SUFFIX: &str = ".json";
const DATE_FIELDS: &[&str] = &[
    "day",
    "timestamp",
    "recorded_at",
    "start_datetime",
    "end_datetime",
    "bedtime_start",
    "bedtime_end",
];

#[derive(omnifs_sdk::Endpoint)]
#[endpoint(
    base = "https://api.ouraring.com",
    default_header = "Accept: application/json"
)]
struct Api;

const AUTH: Auth = Auth::new(
    &["api.ouraring.com"],
    "oauth",
    &[(
        "oauth",
        Scheme::Oauth(OAuth::client_side_token(
            "Oura OAuth",
            "https://cloud.ouraring.com/oauth/authorize",
            "https://api.ouraring.com/oauth/token",
            "http://localhost:58880/",
        )
        .client_id("9443bed5-98df-4a2d-b08e-d2a10c1851ae")
        .scopes(&[
            "email", "personal", "daily", "heartrate", "workout", "tag", "session", "spo2Daily",
        ])
        .summary(
            "Browser sign-in through omnifs's Oura app; the access token returns directly in the redirect.",
        ),
        ),
    )],
);

#[omnifs_sdk::provider(
    id = "oura",
    display_name = "Oura",
    mount = "oura",
    capabilities(
        domain(
            "api.ouraring.com",
            "Fetch Oura API v2 user collection resources such as sleep, activity, readiness, heart rate, and device data."
        ),
        memory_mb(128, "Leave room for date-range and time-series JSON responses."),
    ),
    auth = AUTH
)]
impl OuraProvider {
    fn start(r: &mut Router) -> Result<()> {
        r.dir("/").handler(root)?;
        r.dir("/{day}").handler(DayKey::entries)?;
        r.file_object::<DailyCollection>("/{day}/{collection}", |o| {
            o.dynamic();
            // The anchor IS this file: declare the single canonical face
            // directly on the block.
            o.canonical::<Json>()?;
            Ok(())
        })?;
        Ok(())
    }
}

async fn root(_cx: DirCx) -> Result<DirProjection> {
    Ok(DirProjection::open(core::iter::empty::<Entry>()))
}

#[omnifs_sdk::path_segment]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    strum::VariantArray,
    strum::EnumString,
    strum::AsRefStr,
    strum::Display,
    strum::IntoStaticStr,
    strum::EnumProperty,
)]
#[strum(serialize_all = "snake_case")]
enum Collection {
    #[strum(serialize = "daily_activity.json")]
    DailyActivity,
    #[strum(serialize = "daily_cardiovascular_age.json")]
    DailyCardiovascularAge,
    #[strum(serialize = "daily_readiness.json")]
    DailyReadiness,
    #[strum(serialize = "daily_resilience.json")]
    DailyResilience,
    #[strum(serialize = "daily_sleep.json")]
    DailySleep,
    #[strum(serialize = "daily_spo2.json")]
    DailySpo2,
    #[strum(serialize = "daily_stress.json")]
    DailyStress,
    #[strum(serialize = "enhanced_tag.json")]
    EnhancedTag,
    #[strum(serialize = "heart_rate.json", props(endpoint = "heartrate"))]
    HeartRate,
    #[strum(serialize = "rest_mode_period.json")]
    RestModePeriod,
    #[strum(serialize = "ring_battery_level.json")]
    RingBatteryLevel,
    #[strum(serialize = "session.json")]
    Session,
    #[strum(serialize = "sleep.json")]
    Sleep,
    #[strum(serialize = "sleep_time.json")]
    SleepTime,
    #[strum(serialize = "tag.json")]
    Tag,
    #[strum(serialize = "vo2_max.json", props(endpoint = "vO2_max"))]
    Vo2Max,
    #[strum(serialize = "workout.json")]
    Workout,
}

impl Collection {
    fn endpoint(self) -> &'static str {
        if let Some(endpoint) = self.get_str("endpoint") {
            return endpoint;
        }
        let name: &'static str = self.into();
        name.strip_suffix(JSON_SUFFIX).unwrap_or(name)
    }

    fn range_kind(self) -> RangeKind {
        match self {
            Self::HeartRate | Self::RingBatteryLevel => RangeKind::DateTime,
            _ => RangeKind::Date,
        }
    }

    fn entries() -> impl Iterator<Item = Entry> {
        Self::VARIANTS.iter().map(|collection| {
            let name: &'static str = (*collection).into();
            Entry::file(name)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RangeKind {
    Date,
    DateTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Day(Date);

impl Day {
    fn preload_range(self) -> Result<RangeInclusive<Self>> {
        Ok(self.checked_add(-PRELOAD_RADIUS)?..=self.checked_add(PRELOAD_RADIUS)?)
    }

    fn checked_add(self, duration: Duration) -> Result<Self> {
        let date = self
            .0
            .checked_add(duration)
            .ok_or_else(|| ProviderError::invalid_input("Oura date is out of range"))?;
        Self::from_date(date)
            .ok_or_else(|| ProviderError::invalid_input("Oura date is out of range"))
    }

    fn next(self) -> Option<Self> {
        self.0.next_day().and_then(Self::from_date)
    }

    fn from_date(date: Date) -> Option<Self> {
        (0..=9999).contains(&date.year()).then_some(Self(date))
    }

    fn start_datetime(self) -> Result<String> {
        self.datetime(Time::MIDNIGHT)
    }

    fn end_datetime(self) -> Result<String> {
        let end = Time::from_hms(23, 59, 59).map_err(|error| {
            ProviderError::internal(format!("Oura end-of-day time construction failed: {error}"))
        })?;
        self.datetime(end)
    }

    fn datetime(self, time: Time) -> Result<String> {
        self.0
            .with_time(time)
            .assume_utc()
            .format(&Rfc3339)
            .map_err(|error| ProviderError::internal(format!("Oura datetime format: {error}")))
    }

    fn through(self, end: Self) -> impl Iterator<Item = Self> {
        std::iter::successors(Some(self), move |day| {
            (*day != end).then(|| day.next()).flatten()
        })
    }
}

impl FromStr for Day {
    type Err = ();

    fn from_str(segment: &str) -> std::result::Result<Self, Self::Err> {
        Date::parse(segment, &Iso8601::DATE)
            .ok()
            .and_then(Self::from_date)
            .ok_or(())
    }
}

impl fmt::Display for Day {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl PathSegment for Day {}

#[omnifs_sdk::path_captures]
struct DayKey {
    day: Day,
}

impl DayKey {
    #[allow(clippy::unused_self)]
    fn entries(self, _cx: DirCx) -> Result<DirProjection> {
        Ok(DirProjection::exhaustive(Collection::entries()))
    }
}

#[omnifs_sdk::path_captures]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DailyCollectionKey {
    day: Day,
    collection: Collection,
}

/// One day's slice of an Oura usercollection, served as a single JSON file at
/// `/{day}/{collection}`. A single ranged fetch materializes a whole window of
/// neighboring days; the requested day is the object's canonical and the rest
/// ride along as same-type sibling preloads.
#[omnifs_sdk::object(
    kind = "daily_collection",
    key = DailyCollectionKey
)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
struct DailyCollection(Value);

impl DailyCollection {
    async fn load(
        cx: &Cx,
        key: &DailyCollectionKey,
        _since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let response = RangeRequest {
            collection: key.collection,
            range: key.day.preload_range()?,
        }
        .fetch(cx)
        .await?;
        response.into_load(key.day)
    }
}

#[derive(Debug)]
struct RangeRequest {
    collection: Collection,
    range: RangeInclusive<Day>,
}

impl RangeRequest {
    async fn fetch(self, cx: &Cx) -> Result<RangeResponse> {
        let mut request = cx
            .endpoint(Api)
            .get(format!("/v2/usercollection/{}", self.collection.endpoint()));
        match self.collection.range_kind() {
            RangeKind::Date => {
                request = request
                    .query("start_date", self.range.start())
                    .query("end_date", self.range.end());
            },
            RangeKind::DateTime => {
                request = request
                    .query("start_datetime", self.range.start().start_datetime()?)
                    .query("end_datetime", self.range.end().end_datetime()?);
            },
        }

        let response = request.send_checked().await?;
        let body: Value = serde_json::from_slice(response.body())
            .map_err(|error| ProviderError::internal(format!("Oura JSON parse error: {error}")))?;
        Ok(RangeResponse {
            collection: self.collection,
            range: self.range,
            validator: response.header("etag").map(Validator::from),
            body,
        })
    }
}

#[derive(Debug)]
struct RangeResponse {
    collection: Collection,
    range: RangeInclusive<Day>,
    validator: Option<Validator>,
    body: Value,
}

impl RangeResponse {
    /// Assemble the requested day as the loaded object's canonical and ride the
    /// neighboring days back as same-type sibling preloads (R5): one fetch warms
    /// the whole window's canonical cache, so reading an adjacent day is a warm
    /// hit. Every day's slice is materialized exactly once from the partitioned
    /// rows.
    fn into_load(self, requested: Day) -> Result<Load<DailyCollection>> {
        let mut grouping = self.group_by_day();
        let (value, canonical) = self.day_canonical(&mut grouping, requested)?;
        let mut load = Load::fresh(value, canonical);
        for day in self.range.start().through(*self.range.end()) {
            if day == requested {
                continue;
            }
            let (_, canonical) = self.day_canonical(&mut grouping, day)?;
            load = load.preload_object(ObjectEntry::fresh(
                DailyCollectionKey {
                    day,
                    collection: self.collection,
                },
                canonical,
            ));
        }
        Ok(load)
    }

    /// Materialize one day's slice and its verbatim canonical bytes.
    fn day_canonical(
        &self,
        grouping: &mut DayGrouping,
        day: Day,
    ) -> Result<(DailyCollection, Canonical)> {
        let value = grouping.take(day);
        let canonical = self.canonical(&value)?;
        Ok((DailyCollection(value), canonical))
    }

    fn canonical(&self, value: &Value) -> Result<Canonical> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| ProviderError::internal(format!("Oura JSON encode error: {error}")))?;
        Ok(Canonical {
            bytes,
            validator: self.validator.clone(),
        })
    }

    /// Partition the response's `data` rows by day in a single pass, so each
    /// row's date is parsed once instead of re-scanning the whole array per day
    /// across the preload window. Responses without a `data` array re-serve the
    /// whole body for every day.
    fn group_by_day(&self) -> DayGrouping {
        let Some(items) = self.body.get("data").and_then(Value::as_array) else {
            return DayGrouping::Whole(self.body.clone());
        };
        let mut rows: HashMap<Day, Vec<Value>> = HashMap::new();
        for item in items {
            if let Some(day) = item_day(item) {
                rows.entry(day).or_default().push(item.clone());
            }
        }
        DayGrouping::Partitioned(rows)
    }
}

/// Source of each day's projected value, materialized once per day from the
/// range response.
enum DayGrouping {
    Partitioned(HashMap<Day, Vec<Value>>),
    Whole(Value),
}

impl DayGrouping {
    fn take(&mut self, day: Day) -> Value {
        match self {
            Self::Partitioned(rows) => {
                serde_json::json!({ "data": rows.remove(&day).unwrap_or_default() })
            },
            Self::Whole(body) => body.clone(),
        }
    }
}

fn item_day(item: &Value) -> Option<Day> {
    DATE_FIELDS.iter().find_map(|field| {
        item.get(*field)
            .and_then(Value::as_str)
            .and_then(date_value)
    })
}

fn date_value(value: &str) -> Option<Day> {
    let date = if value.len() == 10 {
        Date::parse(value, &Iso8601::DATE).ok()?
    } else {
        Date::parse(value.get(..10)?, &Iso8601::DATE).ok()?
    };
    Day::from_date(date)
}
