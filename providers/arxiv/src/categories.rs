use omnifs_sdk::prelude::*;

use crate::api::fetch_listing;
use crate::paper_subtree::PaperSubtree;
use crate::query::{
    EARLIEST_YEAR, SortAxis, and, author_query, category_month_query, category_query,
    current_year_utc, listing_url, window_start,
};
use crate::selector::{empty_exhaustive_projection, window_index_projection};
use crate::types::{CategoryKey, EncodedSelector, PaperKey, YearMonth};
use crate::{Result, State};

pub struct CategoryHandlers;

#[handlers]
impl CategoryHandlers {
    /// `/categories/{cat}` projects every calendar bucket newest-first
    /// alongside the `new/`, `updated/`, and `by-author/` axes (which
    /// auto-derive as static children from the routes below).
    #[dir("/categories/{category}")]
    fn category_root(_cx: &DirCx<State>, _category: CategoryKey) -> Result<Projection> {
        let mut p = Projection::new();
        for year in (EARLIEST_YEAR..=current_year_utc()).rev() {
            for month in (1..=12u32).rev() {
                p.dir(format!("{year:04}-{month:02}"));
            }
        }
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[dir("/categories/{category}/{ym}")]
    async fn category_month(
        cx: &DirCx<State>,
        category: CategoryKey,
        ym: YearMonth,
    ) -> Result<Projection> {
        let url = listing_url(&category_month_query(&category, ym), SortAxis::Submitted, 0);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("categories/{category}/{ym}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/categories/{category}/{ym}/{paper}")]
    fn category_month_paper(
        _cx: &Cx<State>,
        _category: CategoryKey,
        _ym: YearMonth,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }

    #[dir("/categories/{category}/new")]
    fn category_new_index(_cx: &DirCx<State>, _category: CategoryKey) -> Result<Projection> {
        Ok(window_index_projection())
    }

    #[dir("/categories/{category}/new/{n}")]
    async fn category_new_window(
        cx: &DirCx<State>,
        category: CategoryKey,
        n: u32,
    ) -> Result<Projection> {
        let start = window_start(n)?;
        let url = listing_url(&category_query(&category), SortAxis::Submitted, start);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("categories/{category}/new/{n}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/categories/{category}/new/{n}/{paper}")]
    fn category_new_paper(
        _cx: &Cx<State>,
        _category: CategoryKey,
        _n: u32,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }

    #[dir("/categories/{category}/updated")]
    fn category_updated_index(_cx: &DirCx<State>, _category: CategoryKey) -> Result<Projection> {
        Ok(window_index_projection())
    }

    #[dir("/categories/{category}/updated/{n}")]
    async fn category_updated_window(
        cx: &DirCx<State>,
        category: CategoryKey,
        n: u32,
    ) -> Result<Projection> {
        let start = window_start(n)?;
        let url = listing_url(&category_query(&category), SortAxis::Updated, start);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("categories/{category}/updated/{n}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/categories/{category}/updated/{n}/{paper}")]
    fn category_updated_paper(
        _cx: &Cx<State>,
        _category: CategoryKey,
        _n: u32,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }

    #[dir("/categories/{category}/by-author")]
    fn category_by_author_root(_cx: &DirCx<State>, _category: CategoryKey) -> Result<Projection> {
        Ok(empty_exhaustive_projection())
    }

    #[dir("/categories/{category}/by-author/{author}")]
    async fn category_by_author(
        cx: &DirCx<State>,
        category: CategoryKey,
        author: EncodedSelector,
    ) -> Result<Projection> {
        let decoded = author.decode()?;
        let q = and(&category_query(&category), &author_query(&decoded));
        let url = listing_url(&q, SortAxis::Submitted, 0);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("categories/{category}/by-author/{author}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/categories/{category}/by-author/{author}/{paper}")]
    fn category_by_author_paper(
        _cx: &Cx<State>,
        _category: CategoryKey,
        _author: EncodedSelector,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }
}
