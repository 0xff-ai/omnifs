use omnifs_sdk::prelude::*;
use serde_json::{Map, Value, json};

use crate::api::{download_pdf, download_source, fetch_paper_detail};
use crate::query::{paper_abs_url, paper_pdf_url, paper_source_url};
use crate::types::{PaperKey, ParsedEntry, VersionKey};
use crate::{Result, State};

pub(crate) async fn project_paper_dir(cx: &Cx<State>, paper: &PaperKey) -> Result<Projection> {
    // `paper.pdf`, `source.tar.gz`, `links.json`, and `versions` are
    // auto-derived from sibling `#[file]`/`#[dir]` handlers in each
    // scope module; this handler returns the two eager JSON children
    // owned by this directory.
    let entry = load_entry(cx, paper).await?;
    let mut p = Projection::new();
    entry.write_metadata_json(&mut p, None);
    p.file_with_content("links.json", entry.links_json_bytes(None));
    p.page(PageStatus::Exhaustive);
    Ok(p)
}

pub(crate) async fn project_versions_dir(cx: &Cx<State>, paper: &PaperKey) -> Result<Projection> {
    let entry = load_entry(cx, paper).await?;
    let mut p = Projection::new();
    for version in 1..=entry.latest_version {
        p.dir(format!("v{version}"));
    }
    p.page(PageStatus::Exhaustive);
    Ok(p)
}

pub(crate) async fn project_version_dir(
    cx: &Cx<State>,
    paper: &PaperKey,
    version: &VersionKey,
) -> Result<Projection> {
    let entry = load_entry(cx, paper).await?;
    let version = version.number_required()?;
    entry.validate_version(version)?;
    let mut p = Projection::new();
    entry.write_metadata_json(&mut p, Some(version));
    p.file_with_content("links.json", entry.links_json_bytes(Some(version)));
    p.page(PageStatus::Exhaustive);
    Ok(p)
}

pub(crate) async fn read_paper_pdf(
    cx: &Cx<State>,
    paper: &PaperKey,
    version: Option<u32>,
) -> Result<FileContent> {
    let raw_id = paper.decode()?;
    let bytes = download_pdf(cx, &raw_id, version).await?;
    Ok(FileContent::bytes(bytes))
}

pub(crate) async fn read_paper_source(
    cx: &Cx<State>,
    paper: &PaperKey,
    version: Option<u32>,
) -> Result<FileContent> {
    let raw_id = paper.decode()?;
    let bytes = download_source(cx, &raw_id, version).await?;
    Ok(FileContent::bytes(bytes))
}

pub(crate) async fn read_paper_links(
    cx: &Cx<State>,
    paper: &PaperKey,
    version: Option<u32>,
) -> Result<FileContent> {
    let entry = load_entry(cx, paper).await?;
    if let Some(v) = version {
        entry.validate_version(v)?;
    }
    Ok(FileContent::bytes(entry.links_json_bytes(version)))
}

async fn load_entry(cx: &Cx<State>, paper: &PaperKey) -> Result<ParsedEntry> {
    let raw_id = paper.decode()?;
    fetch_paper_detail(cx, &raw_id).await
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

fn pretty_json(payload: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(payload).expect("serializing json! is infallible");
    bytes.push(b'\n');
    bytes
}
