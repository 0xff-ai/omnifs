use omnifs_sdk::prelude::*;

use crate::api::fetch_listing;
use crate::paper_subtree::PaperSubtree;
use crate::query::{SortAxis, and, author_query, category_query, listing_url, window_start};
use crate::selector::window_index_projection;
use crate::types::{CategoryKey, EncodedSelector, PaperKey};
use crate::{Result, State};

pub struct AuthorHandlers;

#[handlers]
impl AuthorHandlers {
    #[dir("/authors/{author}")]
    async fn author_root(cx: &DirCx<State>, author: EncodedSelector) -> Result<Projection> {
        let decoded = author.decode()?;
        let url = listing_url(&author_query(&decoded), SortAxis::Submitted, 0);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("authors/{author}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/authors/{author}/{paper}")]
    fn author_paper(
        _cx: &Cx<State>,
        _author: EncodedSelector,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }

    #[dir("/authors/{author}/new")]
    fn author_new_index(_cx: &DirCx<State>, _author: EncodedSelector) -> Result<Projection> {
        Ok(window_index_projection())
    }

    #[dir("/authors/{author}/new/{n}")]
    async fn author_new_window(
        cx: &DirCx<State>,
        author: EncodedSelector,
        n: u32,
    ) -> Result<Projection> {
        let start = window_start(n)?;
        let decoded = author.decode()?;
        let url = listing_url(&author_query(&decoded), SortAxis::Submitted, start);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("authors/{author}/new/{n}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/authors/{author}/new/{n}/{paper}")]
    fn author_new_paper(
        _cx: &Cx<State>,
        _author: EncodedSelector,
        _n: u32,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }

    #[dir("/authors/{author}/updated")]
    fn author_updated_index(_cx: &DirCx<State>, _author: EncodedSelector) -> Result<Projection> {
        Ok(window_index_projection())
    }

    #[dir("/authors/{author}/updated/{n}")]
    async fn author_updated_window(
        cx: &DirCx<State>,
        author: EncodedSelector,
        n: u32,
    ) -> Result<Projection> {
        let start = window_start(n)?;
        let decoded = author.decode()?;
        let url = listing_url(&author_query(&decoded), SortAxis::Updated, start);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("authors/{author}/updated/{n}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/authors/{author}/updated/{n}/{paper}")]
    fn author_updated_paper(
        _cx: &Cx<State>,
        _author: EncodedSelector,
        _n: u32,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }

    // `/authors/{author}/by-category` is auto-navigable from the
    // `by-category/{category}` route below; no stub handler needed.

    #[dir("/authors/{author}/by-category/{category}")]
    async fn author_by_category(
        cx: &DirCx<State>,
        author: EncodedSelector,
        category: CategoryKey,
    ) -> Result<Projection> {
        let decoded = author.decode()?;
        let q = and(&author_query(&decoded), &category_query(&category));
        let url = listing_url(&q, SortAxis::Submitted, 0);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("authors/{author}/by-category/{category}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/authors/{author}/by-category/{category}/{paper}")]
    fn author_by_category_paper(
        _cx: &Cx<State>,
        _author: EncodedSelector,
        _category: CategoryKey,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }
}
