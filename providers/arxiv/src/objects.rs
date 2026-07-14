//! arXiv paper object, representations, and warm projections.

use omnifs_sdk::prelude::*;
use serde::Deserialize;
use serde_json::json;

use crate::PaperKey;
use crate::PaperVersionKey;
use crate::api::{
    download_pdf, download_source, load_paper, paper_abs_url, paper_pdf_url, paper_source_url,
    parse_paper_atom,
};

#[omnifs_sdk::object(
    kind = "arxiv.paper",
    key = PaperVersionKey,
    canonical = Atom,
    decode = parse_paper_atom
)]
#[derive(Clone, Debug, Deserialize)]
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

impl Paper {
    pub(crate) async fn load(
        cx: &Cx<()>,
        key: &PaperVersionKey,
        since: Option<Validator>,
    ) -> Result<Load<Self>> {
        let loaded = load_paper(cx, key.paper.decoded(), since).await?;
        if let Load::Fresh { ref value, .. } = loaded
            && let Some(version) = key.version.number()
        {
            value.validate_version(version)?;
        }
        Ok(loaded)
    }

    pub(crate) fn validate_version(&self, version: u32) -> Result<()> {
        if version == 0 || version > self.latest_version {
            return Err(ProviderError::not_found("paper version not found"));
        }
        Ok(())
    }

    pub(crate) fn metadata_json(&self, key: &PaperVersionKey) -> Result<FileProjection> {
        if let Some(version) = key.version.number() {
            self.validate_version(version)?;
        }
        Ok(
            FileProjection::inline(self.metadata_json_bytes(key.version.number())?)
                .content_type(ContentType::Json)
                .build(),
        )
    }

    pub(crate) fn metadata_json_bytes(&self, version: Option<u32>) -> Result<Vec<u8>> {
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

    pub(crate) async fn pdf(cx: Cx<()>, key: PaperVersionKey) -> Result<BlobFile<Atom>> {
        let paper_key = key.paper_key();
        let paper = loaded_paper(&cx, &paper_key).await?;
        let version = key.version.number();
        if let Some(v) = version {
            paper.validate_version(v)?;
        }
        let blob = download_pdf(&cx, key.paper.decoded(), version).await?;
        Ok(BlobFile::new(blob.id)
            .size(Size::Exact(blob.size))
            // A numbered version (`v3`) is immutable; `@latest` aliases whichever
            // version is current, so its bytes can change under the stable path.
            .stability(if key.version.is_numbered() {
                Stability::Stable
            } else {
                Stability::Dynamic
            })
            .content_type(ContentType::Custom("application/pdf")))
    }

    pub(crate) async fn source(cx: Cx<()>, key: PaperVersionKey) -> Result<BlobFile<Atom>> {
        let paper_key = key.paper_key();
        let paper = loaded_paper(&cx, &paper_key).await?;
        let version = key.version.number();
        if let Some(v) = version {
            paper.validate_version(v)?;
        }
        let blob = download_source(&cx, key.paper.decoded(), version).await?;
        Ok(BlobFile::new(blob.id)
            .size(Size::Exact(blob.size))
            .stability(if key.version.is_numbered() {
                Stability::Stable
            } else {
                Stability::Dynamic
            })
            .content_type(ContentType::Octet))
    }

    /// `v1..=latest_version`, derived from the loaded Atom. No callout.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn version_dirs(paper: &Paper) -> Result<DirListing> {
        Ok(DirListing::exhaustive(
            std::iter::once(Entry::dir("@latest"))
                .chain((1..=paper.latest_version).map(|v| Entry::dir(format!("v{v}")))),
        ))
    }
}

pub(crate) async fn loaded_paper(cx: &Cx<()>, key: &PaperKey) -> Result<Paper> {
    let version_key = PaperVersionKey {
        paper: key.paper.clone(),
        version: Facet(crate::PaperVersion::latest()),
    };
    match Paper::load(cx, &version_key, None).await? {
        Load::Fresh { value, .. } => Ok(value),
        Load::Unchanged => Err(ProviderError::internal(
            "paper unchanged without a host-pushed canonical in this handler path",
        )),
        Load::NotFound => Err(ProviderError::not_found("paper not found")),
    }
}
