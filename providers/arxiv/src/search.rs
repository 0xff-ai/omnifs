use omnifs_sdk::prelude::*;

use crate::api::fetch_listing;
use crate::paper_subtree::PaperSubtree;
use crate::query::{SortAxis, listing_url, window_start};
use crate::types::{EncodedSelector, PaperKey};
use crate::{Result, State};

pub struct SearchHandlers;

#[handlers]
impl SearchHandlers {
    #[dir("/search/{query}")]
    async fn search_root(cx: &DirCx<State>, query: EncodedSelector) -> Result<Projection> {
        let decoded = query.decode()?;
        let url = listing_url(&decoded, SortAxis::Submitted, 0);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("search/{query}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/search/{query}/{paper}")]
    fn search_paper(
        _cx: &Cx<State>,
        _query: EncodedSelector,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }

    #[dir("/search/{query}/new")]
    async fn search_new_index(cx: &DirCx<State>, query: EncodedSelector) -> Result<Projection> {
        let decoded = query.decode()?;
        let url = listing_url(&decoded, SortAxis::Submitted, 0);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("search/{query}/new");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/search/{query}/new/{paper}")]
    fn search_new_index_paper(
        _cx: &Cx<State>,
        _query: EncodedSelector,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }

    #[dir("/search/{query}/new/{n}")]
    async fn search_new_window(
        cx: &DirCx<State>,
        query: EncodedSelector,
        n: u32,
    ) -> Result<Projection> {
        let start = window_start(n)?;
        let decoded = query.decode()?;
        let url = listing_url(&decoded, SortAxis::Submitted, start);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("search/{query}/new/{n}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/search/{query}/new/{n}/{paper}")]
    fn search_new_paper(
        _cx: &Cx<State>,
        _query: EncodedSelector,
        _n: u32,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }

    #[dir("/search/{query}/updated")]
    async fn search_updated_index(cx: &DirCx<State>, query: EncodedSelector) -> Result<Projection> {
        let decoded = query.decode()?;
        let url = listing_url(&decoded, SortAxis::Updated, 0);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("search/{query}/updated");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/search/{query}/updated/{paper}")]
    fn search_updated_index_paper(
        _cx: &Cx<State>,
        _query: EncodedSelector,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }

    #[dir("/search/{query}/updated/{n}")]
    async fn search_updated_window(
        cx: &DirCx<State>,
        query: EncodedSelector,
        n: u32,
    ) -> Result<Projection> {
        let start = window_start(n)?;
        let decoded = query.decode()?;
        let url = listing_url(&decoded, SortAxis::Updated, start);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("search/{query}/updated/{n}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/search/{query}/updated/{n}/{paper}")]
    fn search_updated_paper(
        _cx: &Cx<State>,
        _query: EncodedSelector,
        _n: u32,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }
}
