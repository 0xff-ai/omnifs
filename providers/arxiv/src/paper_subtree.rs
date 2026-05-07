//! Typed subtree handler for the per-paper subtree, mounted under all
//! four scope prefixes (`/papers`, `/categories/.../{paper}`,
//! `/authors/{author}/{paper}`, `/search/{query}/{paper}`) via
//! `#[bind(...)]` sites in the respective scope modules.

use omnifs_sdk::prelude::*;

use crate::paper::{
    project_paper_dir, project_version_dir, project_versions_dir, read_paper_links, read_paper_pdf,
    read_paper_source,
};
use crate::types::{PaperKey, VersionKey};
use crate::{Result, State};

pub struct PaperSubtree {
    pub paper: PaperKey,
}

#[subtree]
impl PaperSubtree {
    #[dir("/")]
    async fn root(cx: &BindCtx<'_, State, PaperSubtree>) -> Result<Projection> {
        project_paper_dir(cx, &cx.bindings().paper).await
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
        read_paper_links(cx, &cx.bindings().paper, None).await
    }

    #[dir("/versions")]
    async fn versions(cx: &BindCtx<'_, State, PaperSubtree>) -> Result<Projection> {
        project_versions_dir(cx, &cx.bindings().paper).await
    }

    #[dir("/versions/{version}")]
    async fn version(
        cx: &BindCtx<'_, State, PaperSubtree>,
        version: VersionKey,
    ) -> Result<Projection> {
        project_version_dir(cx, &cx.bindings().paper, &version).await
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
        read_paper_links(cx, &cx.bindings().paper, Some(version.number_required()?)).await
    }
}
