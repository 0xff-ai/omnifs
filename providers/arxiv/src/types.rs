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

/// `YYYY-MM` calendar bucket. Replaces the prior `{year}/{month}` two-segment path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct YearMonth {
    pub year: u32,
    pub month: u32,
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

impl YearMonth {
    pub(crate) fn is_valid(value: &str) -> bool {
        let bytes = value.as_bytes();
        bytes.len() == 7
            && bytes[4] == b'-'
            && bytes[..4].iter().all(u8::is_ascii_digit)
            && bytes[5..].iter().all(u8::is_ascii_digit)
            && {
                let month: u32 = value[5..].parse().unwrap_or(0);
                (1..=12).contains(&month)
            }
    }
}

impl FromStr for YearMonth {
    type Err = ();
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if !Self::is_valid(value) {
            return Err(());
        }
        let year = value[..4].parse().map_err(|_| ())?;
        let month = value[5..].parse().map_err(|_| ())?;
        Ok(Self { year, month })
    }
}

impl fmt::Display for YearMonth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}", self.year, self.month)
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
impl_string_newtype!(EncodedSelector);
impl_string_newtype!(PaperKey);
impl_string_newtype!(VersionKey);
