use std::sync::LazyLock;

use omnifs_sdk::prelude::*;
use serde_json::{Map, Value, json};
use url::Url;

use crate::api::{download_pdf, download_source, fetch_paper_detail};
use crate::types::{CategoryKey, PaperKey, ParsedEntry, VersionKey, pretty_json};
use crate::{Result, State};

const ABS_BASE: &str = "https://arxiv.org/abs";
const PDF_BASE: &str = "https://arxiv.org/pdf";
const SOURCE_BASE: &str = "https://arxiv.org/e-print";

static ABS_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(ABS_BASE).expect("static URL is valid"));
static PDF_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(PDF_BASE).expect("static URL is valid"));
static SOURCE_URL: LazyLock<Url> =
    LazyLock::new(|| Url::parse(SOURCE_BASE).expect("static URL is valid"));

pub struct PaperHandlers;

pub(crate) struct PaperSubtree {
    pub(crate) paper: PaperKey,
    category_hint: Option<CategoryKey>,
}

impl PaperSubtree {
    pub(crate) fn from_category(category: CategoryKey, paper: PaperKey) -> Self {
        Self {
            paper,
            category_hint: Some(category),
        }
    }

    fn direct(paper: PaperKey) -> Self {
        Self {
            paper,
            category_hint: None,
        }
    }
}

#[handlers]
impl PaperHandlers {
    #[dir("/papers")]
    #[allow(clippy::unnecessary_wraps)]
    fn papers_root(_cx: &DirCx<State>) -> Result<Projection> {
        let mut p = Projection::new();
        p.page(PageStatus::More(Cursor::Opaque("paper".to_string())));
        Ok(p)
    }

    #[bind("/papers/{paper}")]
    #[allow(clippy::unnecessary_wraps)]
    fn paper(_cx: &Cx<State>, paper: PaperKey) -> Result<PaperSubtree> {
        Ok(PaperSubtree::direct(paper))
    }
}

#[subtree]
impl PaperSubtree {
    #[dir("/")]
    async fn root(cx: &BindCtx<'_, State, PaperSubtree>) -> Result<Projection> {
        project_paper_dir(cx, cx.bindings()).await
    }

    #[file("/paper.pdf")]
    async fn pdf(cx: &BindCtx<'_, State, PaperSubtree>) -> Result<FileContent> {
        read_paper_pdf(cx, &cx.bindings().paper, None).await
    }

    #[file("/source.tar.gz")]
    async fn source(cx: &BindCtx<'_, State, PaperSubtree>) -> Result<FileContent> {
        read_paper_source(cx, &cx.bindings().paper, None).await
    }

    #[file("/links.json")]
    async fn links(cx: &BindCtx<'_, State, PaperSubtree>) -> Result<FileContent> {
        read_paper_links(cx, cx.bindings(), None).await
    }

    #[dir("/versions")]
    async fn versions(cx: &BindCtx<'_, State, PaperSubtree>) -> Result<Projection> {
        project_versions_dir(cx, cx.bindings()).await
    }

    #[dir("/versions/{version}")]
    async fn version(
        cx: &BindCtx<'_, State, PaperSubtree>,
        version: VersionKey,
    ) -> Result<Projection> {
        project_version_dir(cx, cx.bindings(), &version).await
    }

    #[file("/versions/{version}/paper.pdf")]
    async fn version_pdf(
        cx: &BindCtx<'_, State, PaperSubtree>,
        version: VersionKey,
    ) -> Result<FileContent> {
        read_paper_pdf(cx, &cx.bindings().paper, Some(version.number_required()?)).await
    }

    #[file("/versions/{version}/source.tar.gz")]
    async fn version_source(
        cx: &BindCtx<'_, State, PaperSubtree>,
        version: VersionKey,
    ) -> Result<FileContent> {
        read_paper_source(cx, &cx.bindings().paper, Some(version.number_required()?)).await
    }

    #[file("/versions/{version}/links.json")]
    async fn version_links(
        cx: &BindCtx<'_, State, PaperSubtree>,
        version: VersionKey,
    ) -> Result<FileContent> {
        read_paper_links(cx, cx.bindings(), Some(version.number_required()?)).await
    }
}

async fn project_paper_dir(cx: &Cx<State>, paper: &PaperSubtree) -> Result<Projection> {
    let entry = paper.load_entry(cx).await?;
    let mut p = Projection::new();
    entry.write_metadata_json(&mut p, None);
    p.file_with_content("links.json", entry.links_json_bytes(None));
    p.page(PageStatus::Exhaustive);
    Ok(p)
}

async fn project_versions_dir(cx: &Cx<State>, paper: &PaperSubtree) -> Result<Projection> {
    let entry = paper.load_entry(cx).await?;
    let mut p = Projection::new();
    for version in 1..=entry.latest_version {
        p.dir(format!("v{version}"));
    }
    p.page(PageStatus::Exhaustive);
    Ok(p)
}

async fn project_version_dir(
    cx: &Cx<State>,
    paper: &PaperSubtree,
    version: &VersionKey,
) -> Result<Projection> {
    let entry = paper.load_entry(cx).await?;
    let version = version.number_required()?;
    entry.validate_version(version)?;
    let mut p = Projection::new();
    entry.write_metadata_json(&mut p, Some(version));
    p.file_with_content("links.json", entry.links_json_bytes(Some(version)));
    p.page(PageStatus::Exhaustive);
    Ok(p)
}

async fn read_paper_pdf(
    cx: &Cx<State>,
    paper: &PaperKey,
    version: Option<u32>,
) -> Result<FileContent> {
    let raw_id = paper.decode()?;
    let bytes = download_pdf(cx, &raw_id, version).await?;
    Ok(FileContent::bytes(bytes))
}

async fn read_paper_source(
    cx: &Cx<State>,
    paper: &PaperKey,
    version: Option<u32>,
) -> Result<FileContent> {
    let raw_id = paper.decode()?;
    let bytes = download_source(cx, &raw_id, version).await?;
    Ok(FileContent::bytes(bytes))
}

async fn read_paper_links(
    cx: &Cx<State>,
    paper: &PaperSubtree,
    version: Option<u32>,
) -> Result<FileContent> {
    let entry = paper.load_entry(cx).await?;
    if let Some(v) = version {
        entry.validate_version(v)?;
    }
    Ok(FileContent::bytes(entry.links_json_bytes(version)))
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

impl PaperSubtree {
    async fn load_entry(&self, cx: &Cx<State>) -> Result<ParsedEntry> {
        if let Some(category) = &self.category_hint
            && let Some(entry) = cx.state(|state| {
                state
                    .recent
                    .get(category)
                    .and_then(|index| index.entry(&self.paper))
            })
        {
            return Ok(entry);
        }

        load_entry(cx, &self.paper).await
    }
}

async fn load_entry(cx: &Cx<State>, paper: &PaperKey) -> Result<ParsedEntry> {
    let raw_id = paper.decode()?;
    fetch_paper_detail(cx, &raw_id).await
}

fn paper_resource_url(base: &Url, raw_id: &str, version: Option<u32>, suffix: &str) -> String {
    let mut url = base.clone();
    let (prefix, tail) = raw_id
        .rsplit_once('/')
        .map_or(("", raw_id), |(prefix, tail)| (prefix, tail));
    let mut tail = tail.to_string();
    if let Some(v) = version {
        tail.push('v');
        tail.push_str(&v.to_string());
    }
    tail.push_str(suffix);
    {
        let mut segments = url
            .path_segments_mut()
            .expect("https URLs support path segments");
        for part in prefix.split('/').filter(|part| !part.is_empty()) {
            segments.push(part);
        }
        segments.push(&tail);
    }
    url.into()
}

impl ParsedEntry {
    pub(crate) fn validate_version(&self, version: u32) -> Result<()> {
        if version == 0 || version > self.latest_version {
            return Err(ProviderError::not_found("paper version not found"));
        }
        Ok(())
    }

    pub(crate) fn write_metadata_json(&self, p: &mut Projection, version: Option<u32>) {
        p.file_with_content("metadata.json", self.metadata_json_bytes(version));
    }

    pub(crate) fn metadata_json_bytes(&self, version: Option<u32>) -> Vec<u8> {
        let resolved_version = version.unwrap_or(self.latest_version);
        let payload = json!({
            "raw_arxiv_id": &self.raw_id,
            "current_version": format!("v{resolved_version}"),
            "latest_version": format!("v{}", self.latest_version),
            "published": &self.published,
            "updated": &self.updated,
            "title": &self.title,
            "abstract": &self.abstract_text,
            "authors": &self.authors,
            "primary_category": &self.primary_category,
            "categories": &self.categories,
            "doi": &self.dois,
            "journal_ref": &self.journal_refs,
            "comment": &self.comments,
            "abstract_url": paper_abs_url(&self.raw_id, version),
            "pdf_url": paper_pdf_url(&self.raw_id, version),
            "source_url": paper_source_url(&self.raw_id, version),
        });
        pretty_json(&payload)
    }

    pub(crate) fn links_json_bytes(&self, version: Option<u32>) -> Vec<u8> {
        let mut external = Map::new();
        external.insert(
            "abstract".to_string(),
            Value::String(paper_abs_url(&self.raw_id, version)),
        );
        external.insert(
            "pdf".to_string(),
            Value::String(paper_pdf_url(&self.raw_id, version)),
        );
        external.insert(
            "source".to_string(),
            Value::String(paper_source_url(&self.raw_id, version)),
        );
        let doi_urls: Vec<Value> = self
            .dois
            .iter()
            .map(|doi| {
                let url = if doi.starts_with("http://") || doi.starts_with("https://") {
                    doi.clone()
                } else {
                    format!("https://doi.org/{doi}")
                };
                Value::String(url)
            })
            .collect();
        if !doi_urls.is_empty() {
            external.insert("doi".to_string(), Value::Array(doi_urls));
        }
        pretty_json(&json!({
            "status": "unavailable",
            "provenance": "abs_links_only",
            "items": [],
            "external_links": external,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paper_pdf_url_handles_slash_id() {
        assert_eq!(
            paper_pdf_url("hep-th/9901001", Some(2)),
            "https://arxiv.org/pdf/hep-th/9901001v2.pdf"
        );
    }
}
