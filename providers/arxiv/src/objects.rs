//! arXiv paper object, representations, and warm projections.

use omnifs_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::{paper_abs_url, paper_pdf_url, paper_source_url, parse_paper_atom};
use crate::{PaperVersionKey, pretty_json};

#[omnifs_sdk::object(
    kind = "arxiv.paper",
    key = PaperVersionKey,
    canonical = Atom,
    parse = parse_paper_atom
)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Paper {
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

impl Representable<Json> for Paper {
    fn represent(&self) -> Vec<u8> {
        self.metadata_json_bytes(None)
    }
}

impl Paper {
    pub(crate) fn validate_version(&self, version: u32) -> Result<()> {
        if version == 0 || version > self.latest_version {
            return Err(ProviderError::not_found("paper version not found"));
        }
        Ok(())
    }

    pub(crate) fn metadata_json_bytes(&self, version: Option<u32>) -> Vec<u8> {
        let resolved_version = version.unwrap_or(self.latest_version);
        let doi_urls: Vec<String> = self
            .dois
            .iter()
            .map(|doi| {
                if doi.starts_with("http://") || doi.starts_with("https://") {
                    doi.clone()
                } else {
                    format!("https://doi.org/{doi}")
                }
            })
            .collect();
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
            "doi_urls": doi_urls,
        });
        pretty_json(&payload)
    }

    /// `v1..=latest_version`, derived from the loaded Atom. No callout.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn version_dirs(paper: &Paper) -> Result<DirProjection> {
        Ok(DirProjection::exhaustive(
            std::iter::once(Entry::dir("@latest"))
                .chain((1..=paper.latest_version).map(|v| Entry::dir(format!("v{v}")))),
        ))
    }
}
