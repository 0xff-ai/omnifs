use std::sync::LazyLock;

use omnifs_sdk::Cx;
use omnifs_sdk::http::{Request, ResponseExt};
use omnifs_sdk::prelude::*;
use serde::Deserialize;
use url::Url;

use crate::types::{
    CategoryKey, CategoryPage, FeedSnapshot, PAGE_SIZE, PagePaper, ParsedEntry, RecentPage,
    normalize_whitespace, split_versioned_id,
};
use crate::{Result, State};

pub(crate) const API_BASE: &str = "https://export.arxiv.org/api/query";

const SORT_ORDER_DESC: &str = "descending";
const SORT_BY_SUBMITTED_DATE: &str = "submittedDate";

static API_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(API_BASE).expect("static URL is valid"));

pub(crate) async fn fetch_category_page(
    cx: &Cx<State>,
    category: &CategoryKey,
    page: RecentPage,
) -> Result<CategoryPage> {
    let request_url = category_page_url(category, page);
    let feed_xml = fetch_bytes(cx, &request_url).await?;
    parse_category_page(page, &feed_xml)
}

pub(crate) fn category_page_url(category: &CategoryKey, page: RecentPage) -> String {
    let mut url = API_URL.clone();
    let search_query = format!("cat:{category}");
    let start = page.start().to_string();
    let max_results = PAGE_SIZE.to_string();
    url.query_pairs_mut()
        .append_pair("search_query", &search_query)
        .append_pair("start", &start)
        .append_pair("max_results", &max_results)
        .append_pair("sortBy", SORT_BY_SUBMITTED_DATE)
        .append_pair("sortOrder", SORT_ORDER_DESC);
    url.to_string()
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
) -> Result<omnifs_sdk::blob::BlobRef> {
    let version_tag = version.map_or_else(|| "latest".to_string(), |v| format!("v{v}"));
    fetch_blob(
        cx,
        &crate::paper::paper_pdf_url(raw_id, version),
        format!("arxiv/papers/{raw_id}/{version_tag}/paper.pdf"),
    )
    .await
}

pub(crate) async fn download_source(
    cx: &Cx<State>,
    raw_id: &str,
    version: Option<u32>,
) -> Result<omnifs_sdk::blob::BlobRef> {
    let version_tag = version.map_or_else(|| "latest".to_string(), |v| format!("v{v}"));
    fetch_blob(
        cx,
        &crate::paper::paper_source_url(raw_id, version),
        format!("arxiv/papers/{raw_id}/{version_tag}/source.tar.gz"),
    )
    .await
}

fn parse_category_page(page: RecentPage, feed_xml: &[u8]) -> Result<CategoryPage> {
    let parsed = ParsedFeed::parse(feed_xml)?;
    let snapshot = parsed
        .feed_updated
        .ok_or_else(|| ProviderError::internal("arXiv feed missing updated timestamp"))?;
    let papers = parsed
        .entries
        .into_iter()
        .map(PagePaper::from_entry)
        .collect::<Result<Vec<_>>>()?;
    Ok(CategoryPage {
        page,
        snapshot,
        total_results: parsed.total_results,
        papers,
    })
}

fn paper_lookup_url(raw_id: &str) -> String {
    let mut url = API_URL.clone();
    url.query_pairs_mut().append_pair("id_list", raw_id);
    url.into()
}

fn arxiv_get(cx: &Cx<State>, url: impl Into<String>) -> Request<'_, State> {
    cx.http()
        .get(url)
        .header("User-Agent", "omnifs-provider-arxiv/0.1.0")
}

async fn fetch_bytes(cx: &Cx<State>, url: &str) -> Result<Vec<u8>> {
    let response = arxiv_get(cx, url).send().await?.error_for_status()?;
    Ok(response.into_body())
}

async fn fetch_blob(
    cx: &Cx<State>,
    url: &str,
    cache_key: String,
) -> Result<omnifs_sdk::blob::BlobRef> {
    arxiv_get(cx, url)
        .into_blob()
        .with_cache_key(cache_key)
        .send()
        .await?
        .error_for_status()
}

#[derive(Debug)]
struct ParsedFeed {
    feed_updated: Option<FeedSnapshot>,
    total_results: u32,
    entries: Vec<ParsedEntry>,
}

#[derive(Debug, Deserialize)]
struct AtomFeed {
    updated: Option<String>,
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
        let feed_updated = feed
            .updated
            .as_deref()
            .map(FeedSnapshot::parse)
            .transpose()?;
        Ok(Self {
            feed_updated,
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
    fn category_page_url_uses_the_single_live_query_shape() {
        let category: CategoryKey = "cs.AI".parse().unwrap();
        let page = RecentPage::new(1);

        let url = category_page_url(&category, page);
        let parsed = Url::parse(&url).unwrap();
        let pairs: Vec<_> = parsed.query_pairs().collect();

        assert_eq!(
            pairs,
            vec![
                ("search_query".into(), "cat:cs.AI".into()),
                ("start".into(), "100".into()),
                ("max_results".into(), "100".into()),
                ("sortBy".into(), "submittedDate".into()),
                ("sortOrder".into(), "descending".into()),
            ]
        );
        assert!(
            !pairs
                .iter()
                .any(|(key, value)| key == "search_query" && value.contains("Date"))
        );
    }

    #[test]
    fn parses_feed_snapshot_and_entry_submission_day() {
        let parsed = parse_category_page(RecentPage::zero(), SAMPLE_FEED).unwrap();

        assert_eq!(parsed.snapshot.as_utc_string(), "2025-01-15T05:00:00Z");
        assert_eq!(parsed.total_results, 1234);
        assert_eq!(parsed.papers.len(), 1);
        assert_eq!(parsed.papers[0].key.as_ref(), "2501.12345");
        assert_eq!(parsed.papers[0].submission.path_segment(), "20250110");
        assert_eq!(parsed.papers[0].entry.latest_version, 2);
        assert_eq!(
            parsed.papers[0].entry.abstract_text,
            "This paper studies the case where whitespace is collapsed."
        );
    }

    #[test]
    fn parses_entry_with_non_contiguous_duplicate_doi() {
        let feed = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"
      xmlns:arxiv="http://arxiv.org/schemas/atom">
  <updated>2026-04-02T00:00:00Z</updated>
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
        let parsed = parse_category_page(RecentPage::zero(), feed.as_bytes()).unwrap();
        assert_eq!(
            parsed.papers[0].entry.dois,
            vec!["10.48550/arXiv.2604.00002", "10.1234/journal.2026.002"]
        );
    }
}
