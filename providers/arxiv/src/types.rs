use core::fmt;
use core::str::FromStr;

use omnifs_sdk::prelude::{ProviderError, Result};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Date, Month, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

pub(crate) const PAGE_SIZE: u32 = 100;
const FS_PATH_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b':');

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CategoryKey(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaperKey(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionKey(String);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct FeedSnapshot(OffsetDateTime);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct RecentPage(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct SubmissionDay(Date);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CategoryPage {
    pub(crate) page: RecentPage,
    pub(crate) snapshot: FeedSnapshot,
    pub(crate) total_results: u32,
    pub(crate) papers: Vec<PagePaper>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PagePaper {
    pub(crate) key: PaperKey,
    pub(crate) submission: SubmissionDay,
    pub(crate) entry: ParsedEntry,
}

/// Immutable view of one parsed arXiv Atom entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ParsedEntry {
    pub raw_id: String,
    pub latest_version: u32,
    pub published: String,
    pub updated: String,
    pub title: String,
    pub abstract_text: String,
    pub authors: Vec<String>,
    pub primary_category: Option<String>,
    pub categories: Vec<String>,
    pub dois: Vec<String>,
    pub journal_refs: Vec<String>,
    pub comments: Vec<String>,
}

impl CategoryKey {
    pub(crate) fn is_valid(value: &str) -> bool {
        !matches!(
            value,
            "categories" | "papers" | "recent" | "submissions" | "pages"
        ) && !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    }
}

impl PaperKey {
    pub(crate) fn is_valid(value: &str) -> bool {
        if value.is_empty() {
            return false;
        }
        let has_digit = value.bytes().any(|b| b.is_ascii_digit());
        let has_separator = value.contains('.') || value.contains("%2F");
        has_digit
            && has_separator
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'%' | b':')
            })
    }

    pub(crate) fn encode_raw_id(raw_id: &str) -> Self {
        Self(utf8_percent_encode(raw_id, FS_PATH_ENCODE_SET).to_string())
    }

    pub(crate) fn decode(&self) -> Result<String> {
        let decoded = percent_decode_str(self.as_ref())
            .decode_utf8()
            .map_err(|_| ProviderError::not_found("paper id encoding was not valid UTF-8"))?
            .into_owned();
        if decoded.is_empty() {
            return Err(ProviderError::not_found("paper id is empty"));
        }
        let (base, explicit_version) = split_versioned_id(&decoded);
        if explicit_version.is_some() {
            return Err(ProviderError::not_found(
                "versioned paper ids must be accessed through versions/",
            ));
        }
        Ok(base)
    }
}

impl VersionKey {
    pub(crate) fn is_valid(value: &str) -> bool {
        value.len() >= 2
            && value.starts_with('v')
            && value[1..].bytes().all(|byte| byte.is_ascii_digit())
    }

    pub(crate) fn number(&self) -> Option<u32> {
        self.0.strip_prefix('v')?.parse().ok()
    }

    pub(crate) fn number_required(&self) -> Result<u32> {
        self.number()
            .ok_or_else(|| ProviderError::not_found("invalid paper version"))
    }
}

impl FeedSnapshot {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        Ok(Self(parse_atom_datetime(value)?))
    }

    pub(crate) fn utc_date(self) -> Date {
        self.0.date()
    }

    pub(crate) fn as_utc_string(self) -> String {
        format_atom_utc(self.0)
    }
}

impl RecentPage {
    pub(crate) fn new(index: u64) -> Self {
        Self(index)
    }

    pub(crate) fn zero() -> Self {
        Self(0)
    }

    pub(crate) fn index(self) -> u64 {
        self.0
    }

    pub(crate) fn start(self) -> u64 {
        self.0 * u64::from(PAGE_SIZE)
    }

    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl SubmissionDay {
    pub(crate) fn parse_path(segment: &str) -> Result<Self> {
        let date = parse_compact_date(segment)?;
        Ok(Self(date))
    }

    pub(crate) fn from_published(published: &str) -> Result<Self> {
        Ok(Self(parse_atom_datetime(published)?.date()))
    }

    pub(crate) fn date(self) -> Date {
        self.0
    }

    pub(crate) fn path_segment(self) -> String {
        let date = self.date();
        format!(
            "{:04}{:02}{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        )
    }
}

impl PagePaper {
    pub(crate) fn from_entry(entry: ParsedEntry) -> Result<Self> {
        let submission = SubmissionDay::from_published(&entry.published)?;
        let key = PaperKey::encode_raw_id(&entry.raw_id);
        Ok(Self {
            key,
            submission,
            entry,
        })
    }
}

impl FromStr for RecentPage {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(());
        }
        Ok(Self::new(value.parse::<u64>().map_err(|_| ())?))
    }
}

impl FromStr for SubmissionDay {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse_path(value).map_err(|_| ())
    }
}

impl fmt::Display for FeedSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_utc_string().fmt(f)
    }
}

impl fmt::Display for RecentPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for SubmissionDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.path_segment().fmt(f)
    }
}

macro_rules! impl_string_newtype {
    ($name:ident) => {
        impl FromStr for $name {
            type Err = ();

            fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
                Self::is_valid(value)
                    .then(|| Self(value.to_string()))
                    .ok_or(())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

impl_string_newtype!(CategoryKey);
impl_string_newtype!(PaperKey);
impl_string_newtype!(VersionKey);

pub(crate) fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn split_versioned_id(raw_id: &str) -> (String, Option<u32>) {
    let bytes = raw_id.as_bytes();
    let mut split = bytes.len();
    while split > 0 && bytes[split - 1].is_ascii_digit() {
        split -= 1;
    }
    if split == bytes.len() || split == 0 || bytes[split - 1] != b'v' {
        return (raw_id.to_string(), None);
    }
    match raw_id[split..].parse::<u32>() {
        Ok(version) => (raw_id[..split - 1].to_string(), Some(version)),
        Err(_) => (raw_id.to_string(), None),
    }
}

pub(crate) fn pretty_json(payload: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(payload).expect("serializing json! is infallible");
    bytes.push(b'\n');
    bytes
}

fn parse_compact_date(value: &str) -> Result<Date> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ProviderError::not_found("submission day must be YYYYMMDD"));
    }
    let year = value[0..4]
        .parse::<i32>()
        .map_err(|_| ProviderError::not_found("invalid submission year"))?;
    let month = value[4..6]
        .parse::<u8>()
        .map_err(|_| ProviderError::not_found("invalid submission month"))?;
    let day = value[6..8]
        .parse::<u8>()
        .map_err(|_| ProviderError::not_found("invalid submission day"))?;
    let month =
        Month::try_from(month).map_err(|_| ProviderError::not_found("invalid submission month"))?;
    Date::from_calendar_date(year, month, day)
        .map_err(|_| ProviderError::not_found("invalid submission date"))
}

fn parse_atom_datetime(value: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(&normalize_whitespace(value), &Rfc3339)
        .map(|timestamp| timestamp.to_offset(UtcOffset::UTC))
        .map_err(|e| ProviderError::internal(format!("invalid arXiv timestamp: {e}")))
}

fn format_atom_utc(value: OffsetDateTime) -> String {
    let value = value.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
        value.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn category_key_rejects_route_scaffolding_names() {
        assert!("cs.AI".parse::<CategoryKey>().is_ok());
        assert!("hep-th".parse::<CategoryKey>().is_ok());
        assert!("math".parse::<CategoryKey>().is_ok());
        assert!("categories".parse::<CategoryKey>().is_err());
        assert!("papers".parse::<CategoryKey>().is_err());
        assert!("recent".parse::<CategoryKey>().is_err());
        assert!("submissions".parse::<CategoryKey>().is_err());
    }
}
