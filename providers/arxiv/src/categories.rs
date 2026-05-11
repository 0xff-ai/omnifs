use omnifs_sdk::prelude::*;

use crate::api::fetch_listing;
use crate::paper_subtree::PaperSubtree;
use crate::query::{
    EARLIEST_YEAR, SortAxis, and, author_query, category_day_query, category_query,
    current_date_utc, current_year_utc, listing_url, window_start,
};
use crate::types::{
    CategoryKey, DayKey, EncodedSelector, MonthKey, PaperKey, YearKey, YearMonthDay, days_in_month,
};
use crate::{Result, State};

pub struct CategoryHandlers;

#[handlers]
impl CategoryHandlers {
    /// `/categories/{cat}` projects year buckets newest-first
    /// alongside the `new/`, `updated/`, and `by-author/` axes (which
    /// auto-derive as static children from the routes below).
    #[dir("/categories/{category}")]
    fn category_root(_cx: &DirCx<State>, _category: CategoryKey) -> Result<Projection> {
        let mut p = Projection::new();
        for year in (EARLIEST_YEAR..=current_year_utc()).rev() {
            p.dir(format!("{year:04}"));
        }
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[dir("/categories/{category}/{year}")]
    fn category_year(
        _cx: &DirCx<State>,
        _category: CategoryKey,
        year: YearKey,
    ) -> Result<Projection> {
        let year = supported_year(year)?;
        let (current_year, current_month, _) = current_date_utc();
        let max_month = if year == current_year {
            current_month
        } else {
            12
        };

        let mut p = Projection::new();
        for month in (1..=max_month).rev() {
            p.dir(format!("{month:02}"));
        }
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[dir("/categories/{category}/{year}/{month}")]
    fn category_month(
        _cx: &DirCx<State>,
        _category: CategoryKey,
        year: YearKey,
        month: MonthKey,
    ) -> Result<Projection> {
        let year = supported_year(year)?;
        let month = supported_month(year, month)?;
        let (current_year, current_month, current_day) = current_date_utc();
        let max_day = if year == current_year && month == current_month {
            current_day
        } else {
            days_in_month(year, month)?
        };

        let mut p = Projection::new();
        for day in (1..=max_day).rev() {
            p.dir(format!("{day:02}"));
        }
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[dir("/categories/{category}/{year}/{month}/{day}")]
    async fn category_day(
        cx: &DirCx<State>,
        category: CategoryKey,
        year: YearKey,
        month: MonthKey,
        day: DayKey,
    ) -> Result<Projection> {
        let ymd = supported_day(year, month, day)?;
        let url = listing_url(&category_day_query(&category, ymd), SortAxis::Submitted, 0);
        let listing = fetch_listing(cx, url).await?;
        let YearMonthDay { year, month, day } = ymd;
        let prefix = format!("categories/{category}/{year:04}/{month:02}/{day:02}");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/categories/{category}/{year}/{month}/{day}/{paper}")]
    fn category_day_paper(
        _cx: &Cx<State>,
        _category: CategoryKey,
        _year: YearKey,
        _month: MonthKey,
        _day: DayKey,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
    }

    #[dir("/categories/{category}/new")]
    async fn category_new_index(cx: &DirCx<State>, category: CategoryKey) -> Result<Projection> {
        let url = listing_url(&category_query(&category), SortAxis::Submitted, 0);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("categories/{category}/new");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/categories/{category}/new/{paper}")]
    fn category_new_index_paper(
        _cx: &Cx<State>,
        _category: CategoryKey,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
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
    async fn category_updated_index(
        cx: &DirCx<State>,
        category: CategoryKey,
    ) -> Result<Projection> {
        let url = listing_url(&category_query(&category), SortAxis::Updated, 0);
        let listing = fetch_listing(cx, url).await?;
        let prefix = format!("categories/{category}/updated");
        Ok(listing.dir_projection(&prefix))
    }

    #[bind("/categories/{category}/updated/{paper}")]
    fn category_updated_index_paper(
        _cx: &Cx<State>,
        _category: CategoryKey,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree { paper })
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

    // `/categories/{category}/by-author` is auto-navigable (literal
    // segment under a captured parent); the SDK derives it from the
    // `by-author/{author}` route below. No stub handler needed.

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

fn supported_year(year: YearKey) -> Result<u32> {
    let year = year.value();
    if !(EARLIEST_YEAR..=current_year_utc()).contains(&year) {
        return Err(ProviderError::not_found("year is outside the arXiv range"));
    }
    Ok(year)
}

fn supported_month(year: u32, month: MonthKey) -> Result<u32> {
    let month = month.value();
    let (current_year, current_month, _) = current_date_utc();
    if year == current_year && month > current_month {
        return Err(ProviderError::not_found("month is in the future"));
    }
    Ok(month)
}

fn supported_day(year: YearKey, month: MonthKey, day: DayKey) -> Result<YearMonthDay> {
    let year = supported_year(year)?;
    let month = supported_month(year, month)?;
    let ymd = YearMonthDay::new(YearKey::from_value(year), MonthKey::from_value(month), day)?;
    let (current_year, current_month, current_day) = current_date_utc();
    if ymd.year == current_year && ymd.month == current_month && ymd.day > current_day {
        return Err(ProviderError::not_found("day is in the future"));
    }
    Ok(ymd)
}
