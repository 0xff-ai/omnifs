use core::fmt;
use core::str::FromStr;

use omnifs_sdk::prelude::{ProviderError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CategoryKey(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EncodedSelector(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaperKey(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionKey(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct YearKey(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MonthKey(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DayKey(u32);

/// Validated UTC calendar day for a bounded arXiv category listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct YearMonthDay {
    pub year: u32,
    pub month: u32,
    pub day: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Listing {
    pub request_url: String,
    pub total_results: u32,
    pub papers: Vec<ListedPaper>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ListedPaper {
    pub encoded_key: String,
    pub entry: ParsedEntry,
}

/// Immutable view of one parsed arXiv Atom entry. Pure data.
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
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    }
}

impl EncodedSelector {
    pub(crate) fn is_valid(value: &str) -> bool {
        !value.is_empty()
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'%' | b':' | b'+' | b'@')
            })
    }
}

impl PaperKey {
    /// Accept new-style ids (`YYMM.NNNNN[vN]`, contains `.` and digits)
    /// and percent-encoded old-style ids (`archive(.sub)?%2FYYMMNNN[vN]`,
    /// contains `%2F`). The shape requirement excludes reserved literal
    /// path segments like `new`, `updated`, `by-author` from matching
    /// the `{paper}` capture.
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

impl YearKey {
    pub(crate) fn from_value(value: u32) -> Self {
        Self(value)
    }

    pub(crate) fn value(self) -> u32 {
        self.0
    }

    pub(crate) fn is_valid(value: &str) -> bool {
        value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_digit())
    }
}

impl MonthKey {
    pub(crate) fn from_value(value: u32) -> Self {
        Self(value)
    }

    pub(crate) fn value(self) -> u32 {
        self.0
    }

    pub(crate) fn is_valid(value: &str) -> bool {
        value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_digit()) && {
            let month: u32 = value.parse().unwrap_or(0);
            (1..=12).contains(&month)
        }
    }
}

impl DayKey {
    pub(crate) fn value(self) -> u32 {
        self.0
    }

    pub(crate) fn is_valid(value: &str) -> bool {
        value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_digit()) && {
            let day: u32 = value.parse().unwrap_or(0);
            (1..=31).contains(&day)
        }
    }
}

impl YearMonthDay {
    pub(crate) fn new(year: YearKey, month: MonthKey, day: DayKey) -> Result<Self> {
        let year = year.value();
        let month = month.value();
        let day = day.value();
        let days = days_in_month(year, month)?;
        if day > days {
            return Err(ProviderError::not_found("day is outside the month"));
        }
        Ok(Self { year, month, day })
    }
}

pub(crate) fn days_in_month(year: u32, month: u32) -> Result<u32> {
    let month_enum = time::Month::try_from(u8::try_from(month).unwrap_or(0))
        .map_err(|_| ProviderError::not_found("invalid month"))?;
    let year_i32 = i32::try_from(year).map_err(|_| ProviderError::not_found("invalid year"))?;
    Ok(u32::from(month_enum.length(year_i32)))
}

impl FromStr for YearKey {
    type Err = ();
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if !Self::is_valid(value) {
            return Err(());
        }
        Ok(Self(value.parse().map_err(|_| ())?))
    }
}

impl fmt::Display for YearKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}", self.0)
    }
}

macro_rules! impl_two_digit_key {
    ($name:ident) => {
        impl FromStr for $name {
            type Err = ();

            fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
                Self::is_valid(value)
                    .then(|| Self(value.parse().expect("validated decimal segment")))
                    .ok_or(())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:02}", self.0)
            }
        }
    };
}

impl_two_digit_key!(MonthKey);
impl_two_digit_key!(DayKey);

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
impl_string_newtype!(EncodedSelector);
impl_string_newtype!(PaperKey);
impl_string_newtype!(VersionKey);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_segments_parse_fixed_width_values() {
        let year: YearKey = "2026".parse().unwrap();
        let month: MonthKey = "05".parse().unwrap();
        let day: DayKey = "11".parse().unwrap();

        assert_eq!(year.value(), 2026);
        assert_eq!(month.value(), 5);
        assert_eq!(day.value(), 11);
        assert!("5".parse::<MonthKey>().is_err());
        assert!("32".parse::<DayKey>().is_err());
    }

    #[test]
    fn year_month_day_rejects_impossible_dates() {
        let year: YearKey = "2026".parse().unwrap();
        let february: MonthKey = "02".parse().unwrap();
        let bad_day: DayKey = "30".parse().unwrap();

        assert!(YearMonthDay::new(year, february, bad_day).is_err());
    }
}
