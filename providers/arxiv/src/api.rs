//! arXiv upstream access: paper-detail Atom fetch, PDF/source blob fetch, and
//! the canonical resource URL builders.

use std::sync::LazyLock;

use omnifs_sdk::endpoint::BlobHandle;
use omnifs_sdk::error::ProviderErrorKind;
use omnifs_sdk::prelude::*;
use serde::Deserialize;
use url::Url;

use crate::objects::Paper;
use crate::{normalize_whitespace, split_versioned_id};

#[derive(omnifs_sdk::Endpoint)]
#[endpoint(
    base = "https://export.arxiv.org",
    default_header = "User-Agent: omnifs-provider-arxiv"
)]
pub struct ArxivApi;

#[derive(omnifs_sdk::Endpoint)]
#[endpoint(
    base = "https://arxiv.org",
    default_header = "User-Agent: omnifs-provider-arxiv"
)]
pub struct ArxivWeb;

const ABS_BASE: &str = "https://arxiv.org/abs";
const PDF_BASE: &str = "https://arxiv.org/pdf";
const SOURCE_BASE: &str = "https://arxiv.org/e-print";

static ABS_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(ABS_BASE).expect("static URL is valid"));
static PDF_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(PDF_BASE).expect("static URL is valid"));
static SOURCE_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(SOURCE_BASE).expect("static URL is valid"));

/// Fetch one paper's Atom feed and load it as the canonical. The canonical
/// bytes are the raw Atom; `parse_paper_atom` only produces the in-memory value.
pub(crate) async fn load_paper<S>(
    cx: &Cx<S>,
    raw_id: &str,
    since: Option<Validator>,
) -> Result<Load<Paper>> {
    match cx
        .endpoint(ArxivApi)
        .get("/api/query")
        .query("id_list", raw_id)
        .maybe_if_none_match(since.as_ref())
        .load_with(parse_paper_atom_or_not_found)
        .await
    {
        Ok(load) => Ok(load),
        Err(err) if err.kind() == ProviderErrorKind::NotFound => Ok(Load::NotFound),
        Err(err) => Err(err),
    }
}

/// Papers per category-recent page. The cursor carries the page index, so the
/// listing needs no provider state (the host echoes the cursor back).
pub(crate) const CATEGORY_PAGE_SIZE: u32 = 50;

/// Fetch one page of a category's most-recent papers and return the raw entry
/// count alongside their parseable bare arXiv ids. Stateless: pagination is
/// driven by `page` (the listing cursor).
pub(crate) async fn fetch_category_page<S>(
    cx: &Cx<S>,
    category: &str,
    page: u32,
) -> Result<(usize, Vec<String>)> {
    let resp = cx
        .endpoint(ArxivApi)
        .get("/api/query")
        .query("search_query", format!("cat:{category}"))
        .query("start", page * CATEGORY_PAGE_SIZE)
        .query("max_results", CATEGORY_PAGE_SIZE)
        .query("sortBy", "submittedDate")
        .query("sortOrder", "descending")
        .send_checked()
        .await?;
    let entry_ids = category_entry_ids(resp.body())?;
    let raw_count = entry_ids.len();
    let ids = entry_ids
        .into_iter()
        .filter_map(|entry_id| arxiv_id_from_entry_id(&entry_id))
        .collect();
    Ok((raw_count, ids))
}

fn category_entry_ids(feed_xml: &[u8]) -> Result<Vec<String>> {
    let feed: CategoryAtomFeed = quick_xml::de::from_reader(feed_xml)
        .map_err(|e| ProviderError::invalid_input(format!("arXiv feed parse error: {e}")))?;
    Ok(feed.entries.into_iter().map(|entry| entry.id).collect())
}

/// `http://arxiv.org/abs/2401.12345v1` -> `2401.12345` (version stripped).
/// Splits on `/abs/` rather than the last `/` so old-style ids keep their
/// archive prefix (`http://arxiv.org/abs/hep-th/9901001v1` -> `hep-th/9901001`).
fn arxiv_id_from_entry_id(entry_id: &str) -> Option<String> {
    let abs = entry_id.trim().rsplit_once("/abs/").map(|(_, raw)| raw)?;
    let (base, _version) = split_versioned_id(abs);
    (!base.is_empty()).then_some(base)
}

pub(crate) async fn download_pdf<S>(
    cx: &Cx<S>,
    raw_id: &str,
    version: Option<u32>,
) -> Result<BlobHandle> {
    let version_tag = version.map_or_else(|| "latest".to_string(), |v| format!("v{v}"));
    fetch_blob(
        cx,
        &paper_pdf_path(raw_id, version),
        format!("arxiv/papers/{raw_id}/{version_tag}/paper.pdf"),
    )
    .await
}

pub(crate) async fn download_source<S>(
    cx: &Cx<S>,
    raw_id: &str,
    version: Option<u32>,
) -> Result<BlobHandle> {
    let version_tag = version.map_or_else(|| "latest".to_string(), |v| format!("v{v}"));
    fetch_blob(
        cx,
        &paper_source_path(raw_id, version),
        format!("arxiv/papers/{raw_id}/{version_tag}/source.tar.gz"),
    )
    .await
}

async fn fetch_blob<S>(cx: &Cx<S>, path: &str, cache_key: String) -> Result<BlobHandle> {
    cx.endpoint(ArxivWeb)
        .get(path)
        .into_blob()
        .cache_key(cache_key)
        .fetch()
        .await
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

fn paper_pdf_path(raw_id: &str, version: Option<u32>) -> String {
    paper_resource_path("/pdf", raw_id, version, ".pdf")
}

fn paper_source_path(raw_id: &str, version: Option<u32>) -> String {
    paper_resource_path("/e-print", raw_id, version, "")
}

fn paper_resource_path(base: &str, raw_id: &str, version: Option<u32>, suffix: &str) -> String {
    let mut path = base.trim_end_matches('/').to_string();
    for part in paper_resource_segments(raw_id, version, suffix) {
        path.push('/');
        path.push_str(&part);
    }
    path
}

fn paper_resource_url(base: &Url, raw_id: &str, version: Option<u32>, suffix: &str) -> String {
    let mut url = base.clone();
    let parts = paper_resource_segments(raw_id, version, suffix);
    {
        let mut segments = url
            .path_segments_mut()
            .expect("https URLs support path segments");
        segments.extend(parts.iter().map(String::as_str));
    }
    url.into()
}

fn paper_resource_segments(raw_id: &str, version: Option<u32>, suffix: &str) -> Vec<String> {
    let (prefix, tail) = raw_id
        .rsplit_once('/')
        .map_or(("", raw_id), |(prefix, tail)| (prefix, tail));
    let mut tail = tail.to_string();
    if let Some(v) = version {
        tail.push('v');
        tail.push_str(&v.to_string());
    }
    tail.push_str(suffix);
    prefix
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .chain(std::iter::once(tail))
        .collect()
}

fn parse_paper_atom_or_not_found(feed_xml: &[u8]) -> Result<Paper> {
    if category_entry_ids(feed_xml)?.is_empty() {
        return Err(ProviderError::not_found("paper not found"));
    }
    parse_paper_atom(feed_xml)
}

/// Parse a one-entry arXiv Atom feed into a [`Paper`]. The endpoint's
/// `arxiv.org` resource URLs (`paper.pdf`, `source.tar.gz`) are not in the
/// feed; they are derived from the paper id at projection time.
pub(crate) fn parse_paper_atom(feed_xml: &[u8]) -> Result<Paper> {
    let feed: AtomFeed = quick_xml::de::from_reader(feed_xml)
        .map_err(|e| ProviderError::invalid_input(format!("arXiv feed parse error: {e}")))?;
    feed.entries
        .into_iter()
        .next()
        .map(Paper::from_raw)
        .transpose()?
        .ok_or_else(|| ProviderError::not_found("paper not found"))
}

#[derive(Debug, Deserialize)]
struct CategoryAtomFeed {
    #[serde(rename = "entry", default)]
    entries: Vec<CategoryEntry>,
}

#[derive(Debug, Deserialize)]
struct CategoryEntry {
    id: String,
}

#[derive(Debug, Deserialize)]
struct AtomFeed {
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

impl Paper {
    fn from_raw(raw: RawEntry) -> Result<Self> {
        let id_text = normalize_whitespace(&raw.id);
        if id_text.is_empty() {
            return Err(ProviderError::invalid_input("arXiv entry had an empty id"));
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
