use omnifs_sdk::prelude::*;

use crate::paper::PaperSubtree;
use crate::recent::{
    project_fetched, project_recent, project_recent_page, project_recent_pages, project_submission,
    project_submissions,
};
use crate::types::{CategoryKey, PaperKey, RecentPage, SubmissionDay};
use crate::{Result, State};

pub struct CategoryHandlers;

#[handlers]
impl CategoryHandlers {
    #[dir("/categories")]
    #[allow(clippy::unnecessary_wraps)]
    fn categories_root(_cx: &DirCx<State>) -> Result<Projection> {
        let mut p = Projection::new();
        p.page(PageStatus::More(Cursor::Opaque("category".to_string())));
        Ok(p)
    }

    #[dir("/categories/{category}")]
    #[allow(clippy::unnecessary_wraps)]
    fn category_root(_cx: &DirCx<State>, _category: CategoryKey) -> Result<Projection> {
        let mut p = Projection::new();
        p.dir("recent");
        p.dir("submissions");
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[dir("/categories/{category}/recent")]
    fn recent(cx: &DirCx<State>, category: CategoryKey) -> Result<Projection> {
        project_recent(cx, category)
    }

    #[dir("/categories/{category}/recent/_fetched")]
    fn fetched(cx: &DirCx<State>, category: CategoryKey) -> Result<Projection> {
        project_fetched(cx, category)
    }

    #[dir("/categories/{category}/recent/pages")]
    fn recent_pages(cx: &DirCx<State>, category: CategoryKey) -> Result<Projection> {
        project_recent_pages(cx, category)
    }

    #[dir("/categories/{category}/recent/pages/{page}")]
    async fn recent_page(
        cx: &DirCx<State>,
        category: CategoryKey,
        page: RecentPage,
    ) -> Result<Projection> {
        project_recent_page(cx, category, page).await
    }

    #[bind("/categories/{category}/recent/pages/{page}/{paper}")]
    #[allow(clippy::unnecessary_wraps)]
    fn recent_page_paper(
        _cx: &Cx<State>,
        category: CategoryKey,
        _page: RecentPage,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree::from_category(category, paper))
    }

    #[bind("/categories/{category}/recent/_fetched/{paper}")]
    #[allow(clippy::unnecessary_wraps)]
    fn fetched_paper(
        _cx: &Cx<State>,
        category: CategoryKey,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree::from_category(category, paper))
    }

    #[dir("/categories/{category}/submissions")]
    fn submissions(cx: &DirCx<State>, category: CategoryKey) -> Result<Projection> {
        project_submissions(cx, category)
    }

    #[dir("/categories/{category}/submissions/{day}")]
    fn submission_day(
        cx: &DirCx<State>,
        category: CategoryKey,
        day: SubmissionDay,
    ) -> Result<Projection> {
        project_submission(cx, category, day)
    }

    #[bind("/categories/{category}/submissions/{day}/{paper}")]
    #[allow(clippy::unnecessary_wraps)]
    fn submission_paper(
        _cx: &Cx<State>,
        category: CategoryKey,
        _day: SubmissionDay,
        paper: PaperKey,
    ) -> Result<PaperSubtree> {
        Ok(PaperSubtree::from_category(category, paper))
    }
}
