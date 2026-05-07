use omnifs_sdk::Cx;
use omnifs_sdk::http::ResponseExt;
use omnifs_sdk::prelude::*;
use serde::Deserialize;

use crate::State;
use crate::http_ext::ArxivHttpExt;
use crate::query::{
    normalize_whitespace, paper_lookup_url, paper_pdf_url, paper_source_url, split_versioned_id,
};
use crate::types::{ListedPaper, Listing, PaperKey, ParsedEntry};

pub(crate) const API_BASE: &str = "https://export.arxiv.org/api/query";
pub(crate) const ABS_BASE: &str = "https://arxiv.org/abs";
pub(crate) const PDF_BASE: &str = "https://arxiv.org/pdf";
pub(crate) const SOURCE_BASE: &str = "https://arxiv.org/e-print";

pub(crate) async fn fetch_listing(cx: &Cx<State>, request_url: String) -> Result<Listing> {
    let feed_xml = fetch_bytes(cx, &request_url).await?;
    let parsed = ParsedFeed::parse(&feed_xml)?;
    Ok(Listing {
        request_url,
        total_results: parsed.total_results,
        papers: parsed.entries.into_iter().map(ListedPaper::from).collect(),
    })
}

impl From<ParsedEntry> for ListedPaper {
    fn from(entry: ParsedEntry) -> Self {
        let encoded_key = PaperKey::encode_raw_id(&entry.raw_id);
        Self { encoded_key, entry }
    }
}

pub(crate) async fn fetch_paper_detail(cx: &Cx<State>, raw_id: &str) -> Result<ParsedEntry> {
    let request_url = paper_lookup_url(raw_id);
    let feed_xml = fetch_bytes(cx, &request_url).await?;
    let parsed = ParsedFeed::parse(&feed_xml)?;
    parsed
        .entries
        .into_iter()
        .next()
        .ok_or_else(|| ProviderError::not_found("paper not found"))
}

pub(crate) async fn download_pdf(
    cx: &Cx<State>,
    raw_id: &str,
    version: Option<u32>,
) -> Result<Vec<u8>> {
    fetch_bytes(cx, &paper_pdf_url(raw_id, version)).await
}

pub(crate) async fn download_source(
    cx: &Cx<State>,
    raw_id: &str,
    version: Option<u32>,
) -> Result<Vec<u8>> {
    fetch_bytes(cx, &paper_source_url(raw_id, version)).await
}

async fn fetch_bytes(cx: &Cx<State>, url: &str) -> Result<Vec<u8>> {
    let response = cx.arxiv_get(url).send().await?.error_for_status()?;
    Ok(response.into_body())
}

#[derive(Debug)]
struct ParsedFeed {
    total_results: u32,
    entries: Vec<ParsedEntry>,
}

// arXiv responds with an Atom 1.0 feed using two extension namespaces
// (`arxiv:`, `opensearch:`). quick-xml's serde deserializer matches
// element local names by default, so the renames below are local
// names without prefix.

#[derive(Debug, Deserialize)]
struct AtomFeed {
    #[serde(rename = "totalResults", default)]
    total_results: Option<u32>,
    #[serde(rename = "entry", default)]
    entries: Vec<RawEntry>,
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    id: String,
    updated: String,
    published: String,
    title: String,
    summary: String,
    #[serde(rename = "author", default)]
    authors: Vec<RawAuthor>,
    #[serde(rename = "primary_category", default)]
    primary_category: Option<RawTerm>,
    #[serde(rename = "category", default)]
    categories: Vec<RawTerm>,
    /// Some papers carry multiple DOIs (preprint + journal), so we
    /// collect a `Vec` rather than a single value.
    #[serde(rename = "doi", default)]
    dois: Vec<String>,
    #[serde(rename = "journal_ref", default)]
    journal_refs: Vec<String>,
    #[serde(rename = "comment", default)]
    comments: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawAuthor {
    name: String,
}

#[derive(Debug, Deserialize)]
struct RawTerm {
    #[serde(rename = "@term")]
    term: String,
}

impl ParsedFeed {
    fn parse(feed_xml: &[u8]) -> Result<Self> {
        let feed: AtomFeed = quick_xml::de::from_reader(feed_xml)
            .map_err(|e| ProviderError::internal(format!("arXiv feed parse error: {e}")))?;
        let entries = feed
            .entries
            .into_iter()
            .map(ParsedEntry::from_raw)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            total_results: feed.total_results.unwrap_or(0),
            entries,
        })
    }
}

impl ParsedEntry {
    fn from_raw(raw: RawEntry) -> Result<Self> {
        let id_text = normalize_whitespace(&raw.id);
        if id_text.is_empty() {
            return Err(ProviderError::internal("arXiv entry had an empty id"));
        }
        let abs_id = id_text
            .trim()
            .rsplit_once("/abs/")
            .map_or_else(|| id_text.trim().to_string(), |(_, raw)| raw.to_string());
        let (raw_id, latest_version) = split_versioned_id(&abs_id);
        let latest_version = latest_version.unwrap_or(1);

        let primary_category = raw.primary_category.map(|t| normalize_whitespace(&t.term));
        let mut categories: Vec<String> = raw
            .categories
            .into_iter()
            .map(|t| normalize_whitespace(&t.term))
            .collect();
        if let Some(primary) = &primary_category
            && !categories.iter().any(|existing| existing == primary)
        {
            categories.insert(0, primary.clone());
        }

        let authors = raw
            .authors
            .into_iter()
            .map(|a| normalize_whitespace(&a.name))
            .filter(|s| !s.is_empty())
            .collect();

        Ok(Self {
            raw_id,
            latest_version,
            published: normalize_whitespace(&raw.published),
            updated: normalize_whitespace(&raw.updated),
            title: normalize_whitespace(&raw.title),
            abstract_text: normalize_whitespace(&raw.summary),
            authors,
            primary_category,
            categories,
            dois: clean_strings(raw.dois),
            journal_refs: clean_strings(raw.journal_refs),
            comments: clean_strings(raw.comments),
        })
    }
}

fn clean_strings(raw: Vec<String>) -> Vec<String> {
    raw.into_iter()
        .map(|s| normalize_whitespace(&s))
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_FEED: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"
      xmlns:opensearch="http://a9.com/-/spec/opensearch/1.1/"
      xmlns:arxiv="http://arxiv.org/schemas/atom">
  <title>arXiv Query: search_query=cat:cs.AI</title>
  <id>http://arxiv.org/api/query?search_query=cat:cs.AI</id>
  <updated>2025-01-15T00:00:00-05:00</updated>
  <opensearch:totalResults>1234</opensearch:totalResults>
  <opensearch:startIndex>0</opensearch:startIndex>
  <opensearch:itemsPerPage>50</opensearch:itemsPerPage>
  <entry>
    <id>http://arxiv.org/abs/2501.12345v2</id>
    <updated>2025-01-14T18:00:00Z</updated>
    <published>2025-01-10T18:00:00Z</published>
    <title>Some Paper Title with &amp; Ampersand</title>
    <summary>This paper studies the case where    whitespace
      is collapsed.</summary>
    <author><name>Alice Smith</name></author>
    <author><name>Bob Jones</name></author>
    <arxiv:primary_category xmlns:arxiv="http://arxiv.org/schemas/atom" term="cs.AI" scheme="http://arxiv.org/schemas/atom"/>
    <category term="cs.LG" scheme="http://arxiv.org/schemas/atom"/>
    <category term="stat.ML" scheme="http://arxiv.org/schemas/atom"/>
    <arxiv:doi>10.1000/example.2501.12345</arxiv:doi>
    <arxiv:journal_ref>Example Journal, vol. 1 (2025)</arxiv:journal_ref>
    <arxiv:comment>Accepted at NeurIPS 2025. 12 pages, 5 figures.</arxiv:comment>
  </entry>
</feed>"#;

    #[test]
    fn parses_feed_metadata_and_entries() {
        let parsed = ParsedFeed::parse(SAMPLE_FEED).expect("feed parse succeeds");
        assert_eq!(parsed.total_results, 1234);
        assert_eq!(parsed.entries.len(), 1);

        let entry = &parsed.entries[0];
        assert_eq!(entry.raw_id, "2501.12345");
        assert_eq!(entry.latest_version, 2);
        assert_eq!(entry.title, "Some Paper Title with & Ampersand");
        assert_eq!(
            entry.abstract_text,
            "This paper studies the case where whitespace is collapsed."
        );
        assert_eq!(entry.authors, vec!["Alice Smith", "Bob Jones"]);
        assert_eq!(entry.primary_category.as_deref(), Some("cs.AI"));
        assert_eq!(entry.categories, vec!["cs.AI", "cs.LG", "stat.ML"]);
        assert_eq!(entry.dois, vec!["10.1000/example.2501.12345"]);
    }

    #[test]
    fn listed_paper_preserves_parsed_entry_fields() {
        let parsed = ParsedFeed::parse(SAMPLE_FEED).expect("feed parse succeeds");
        let entry = parsed.entries.into_iter().next().unwrap();
        let listed = ListedPaper::from(entry);
        assert_eq!(listed.encoded_key, "2501.12345");
        assert_eq!(listed.entry.raw_id, "2501.12345");
        assert_eq!(listed.entry.latest_version, 2);
    }

    #[test]
    fn parses_entry_with_non_contiguous_duplicate_doi() {
        // Real arXiv feeds sometimes interleave elements between the
        // duplicate `<arxiv:doi>` siblings. quick-xml's serde
        // deserializer treats `Vec<T>` siblings as a sequence only
        // when they are contiguous; otherwise it sees them as
        // duplicate scalar fields and errors. This pins the workaround.
        let feed = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"
      xmlns:arxiv="http://arxiv.org/schemas/atom">
  <entry>
    <id>http://arxiv.org/abs/2604.00002v1</id>
    <updated>2026-04-02T00:00:00Z</updated>
    <published>2026-04-02T00:00:00Z</published>
    <title>Interleaved-DOI Paper</title>
    <summary>DOIs separated by other elements.</summary>
    <author><name>Test Author</name></author>
    <arxiv:doi>10.48550/arXiv.2604.00002</arxiv:doi>
    <arxiv:journal_ref>Some Journal, 2026</arxiv:journal_ref>
    <arxiv:doi>10.1234/journal.2026.002</arxiv:doi>
  </entry>
</feed>"#;
        let parsed = ParsedFeed::parse(feed.as_bytes()).expect("feed parse succeeds");
        let entry = &parsed.entries[0];
        assert_eq!(
            entry.dois,
            vec!["10.48550/arXiv.2604.00002", "10.1234/journal.2026.002"]
        );
    }
}
