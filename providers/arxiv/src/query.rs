use std::borrow::Cow;
use std::sync::LazyLock;

use omnifs_sdk::prelude::*;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use time::OffsetDateTime;
use url::Url;

use crate::api::{ABS_BASE, API_BASE, PDF_BASE, SOURCE_BASE};
use crate::types::{CategoryKey, EncodedSelector, PaperKey, YearMonth};

const DEFAULT_SORT_ORDER: &str = "descending";

static API_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(API_BASE).expect("static URL is valid"));
static ABS_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(ABS_BASE).expect("static URL is valid"));
static PDF_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(PDF_BASE).expect("static URL is valid"));
static SOURCE_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(SOURCE_BASE).expect("static URL is valid"));

/// arXiv started in 1991. We list calendar buckets from this floor up
/// to the current UTC year.
pub(crate) const EARLIEST_YEAR: u32 = 1991;

/// arXiv's documented per-request cap on `max_results`.
pub(crate) const MAX_PAGE_SIZE: u32 = 2000;

/// FS path-segment encode set: RFC 3986 unreserved (`-._~` plus `:`)
/// pass through; everything else is percent-encoded. Used to round-trip
/// arXiv ids and selector values through FUSE paths.
const FS_PATH_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b':');

#[derive(Clone, Copy, Debug)]
pub(crate) enum SortAxis {
    Submitted,
    Updated,
}

impl SortAxis {
    fn arxiv_param(self) -> &'static str {
        match self {
            SortAxis::Submitted => "submittedDate",
            SortAxis::Updated => "lastUpdatedDate",
        }
    }
}

/// arXiv caps `start` at ~30 000; with `max_results=2000` that's 15
/// windows (indices 0..=14).
pub(crate) const MAX_WINDOW_INDEX: u32 = 14;

/// Convert a window index `n` to the arXiv `start` offset, with bounds
/// check so callers reject out-of-range indices.
pub(crate) fn window_start(n: u32) -> Result<u32> {
    if n > MAX_WINDOW_INDEX {
        return Err(ProviderError::not_found(
            "window index out of range (0..=14)",
        ));
    }
    Ok(n * MAX_PAGE_SIZE)
}

/// Build an arXiv listing URL.
pub(crate) fn listing_url(search_query: &str, sort: SortAxis, start: u32) -> String {
    let mut url = API_URL.clone();
    url.query_pairs_mut()
        .append_pair("search_query", search_query)
        .append_pair("start", &start.to_string())
        .append_pair("max_results", &MAX_PAGE_SIZE.to_string())
        .append_pair("sortBy", sort.arxiv_param())
        .append_pair("sortOrder", DEFAULT_SORT_ORDER);
    url.into()
}

pub(crate) fn category_query(category: &CategoryKey) -> String {
    format!("cat:{category}")
}

/// `cat:{category} AND submittedDate:[YYYYMMDD0000 TO YYYYMMDDhhmm]` for
/// a single calendar month.
pub(crate) fn category_month_query(category: &CategoryKey, ym: YearMonth) -> String {
    let month_enum = time::Month::try_from(u8::try_from(ym.month).unwrap_or(0))
        .expect("YearMonth invariant: month is 1..=12");
    let year_i32 = i32::try_from(ym.year).expect("year fits in i32");
    let last_day = month_enum.length(year_i32);
    let YearMonth { year, month } = ym;
    format!(
        "cat:{category} AND \
         submittedDate:[{year:04}{month:02}010000 TO {year:04}{month:02}{last_day:02}2359]"
    )
}

pub(crate) fn author_query(decoded: &str) -> String {
    format!("au:\"{decoded}\"")
}

/// Combine two arXiv search-query atoms with `AND`.
pub(crate) fn and(left: &str, right: &str) -> String {
    format!("({left}) AND ({right})")
}

impl PaperKey {
    pub(crate) fn encode_raw_id(raw_id: &str) -> String {
        utf8_percent_encode(raw_id, FS_PATH_ENCODE_SET).to_string()
    }

    pub(crate) fn decode(&self) -> Result<String> {
        let decoded = percent_decode(self.as_ref())?;
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

impl EncodedSelector {
    /// Decode the URL-safe form back to the user-typed value. Treats `+`
    /// as space (form-encoded convention) and rejects `"` / `\` so the
    /// decoded value is safe to embed inside the arXiv DSL phrase
    /// `au:"..."`.
    pub(crate) fn decode(&self) -> Result<String> {
        let plus_decoded = self.as_ref().replace('+', " ");
        let decoded = percent_decode(&plus_decoded)?;
        let normalized = normalize_whitespace(&decoded);
        if normalized.is_empty() {
            return Err(ProviderError::not_found("selector is empty"));
        }
        if normalized.contains('"') || normalized.contains('\\') {
            return Err(ProviderError::not_found(
                "selector must not contain `\"` or `\\`",
            ));
        }
        Ok(normalized)
    }
}

pub(crate) fn current_year_utc() -> u32 {
    u32::try_from(OffsetDateTime::now_utc().year()).expect("UTC year fits in u32")
}

pub(crate) fn paper_lookup_url(raw_id: &str) -> String {
    let mut url = API_URL.clone();
    url.query_pairs_mut().append_pair("id_list", raw_id);
    url.into()
}

pub(crate) fn paper_abs_url(raw_id: &str, version: Option<u32>) -> String {
    paper_resource_url(&ABS_URL, raw_id, version, "")
}

pub(crate) fn paper_pdf_url(raw_id: &str, version: Option<u32>) -> String {
    paper_resource_url(&PDF_URL, raw_id, version, ".pdf")
}

pub(crate) fn paper_source_url(raw_id: &str, version: Option<u32>) -> String {
    paper_resource_url(&SOURCE_URL, raw_id, version, "")
}

fn paper_resource_url(base: &Url, raw_id: &str, version: Option<u32>, suffix: &str) -> String {
    let mut url = base.clone();
    let parts: Vec<&str> = raw_id.split('/').collect();
    let last = parts.len().saturating_sub(1);
    {
        let mut segments = url
            .path_segments_mut()
            .expect("https URLs support path segments");
        for (i, part) in parts.iter().enumerate() {
            if i < last {
                segments.push(part);
            } else {
                let mut tail = (*part).to_string();
                if let Some(v) = version {
                    tail.push('v');
                    tail.push_str(&v.to_string());
                }
                tail.push_str(suffix);
                segments.push(&tail);
            }
        }
    }
    url.into()
}

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

fn percent_decode(value: &str) -> Result<String> {
    percent_decode_str(value)
        .decode_utf8()
        .map(Cow::into_owned)
        .map_err(|_| ProviderError::not_found("selector encoding was not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_versioned_id_handles_bare_and_versioned() {
        assert_eq!(
            split_versioned_id("2501.12345"),
            ("2501.12345".to_string(), None)
        );
        assert_eq!(
            split_versioned_id("2501.12345v3"),
            ("2501.12345".to_string(), Some(3))
        );
        assert_eq!(
            split_versioned_id("hep-th/9901001"),
            ("hep-th/9901001".to_string(), None)
        );
    }

    #[test]
    fn paper_key_round_trip_preserves_unreserved() {
        let encoded = PaperKey::encode_raw_id("hep-th/9901001");
        assert_eq!(encoded, "hep-th%2F9901001");
        let key: PaperKey = encoded.parse().unwrap();
        assert_eq!(key.decode().unwrap(), "hep-th/9901001");
    }

    #[test]
    fn paper_pdf_url_handles_slash_form_id() {
        assert_eq!(
            paper_pdf_url("hep-th/9901001", Some(2)),
            "https://arxiv.org/pdf/hep-th/9901001v2.pdf"
        );
    }

    #[test]
    fn selector_decode_rejects_quote_and_backslash() {
        let bad: EncodedSelector = "Bobby%20%22Drop%22".parse().unwrap();
        assert!(bad.decode().is_err());
        let bad2: EncodedSelector = "back%5Cslash".parse().unwrap();
        assert!(bad2.decode().is_err());
    }
}
