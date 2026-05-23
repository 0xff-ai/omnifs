use std::collections::{BTreeMap, HashMap, HashSet};

use omnifs_sdk::prelude::*;
use serde_json::json;

use crate::api::fetch_category_page;
use crate::types::{
    CategoryKey, CategoryPage, FeedSnapshot, PagePaper, PaperKey, ParsedEntry, RecentPage,
    SubmissionDay, pretty_json,
};
use crate::{Result, State};

const MAX_STORED_RECENT_PAGES: usize = 20;

#[derive(Debug, Clone, Default)]
pub(crate) struct RecentIndex {
    feed_updated: Option<FeedSnapshot>,
    total_results: u32,
    pages: BTreeMap<RecentPage, Vec<PaperKey>>,
    // Entries stay with the scan index so paths derived from a category feed can
    // project paper files without a second arXiv lookup for the same payload.
    entries: HashMap<PaperKey, ParsedEntry>,
    buckets: BTreeMap<SubmissionDay, BucketState>,
    contiguous_through: Option<RecentPage>,
    oldest_contiguous_submission: Option<SubmissionDay>,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct BucketState {
    papers: Vec<PaperKey>,
    complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScanInvalidation {
    None,
    NewSnapshot {
        previous: FeedSnapshot,
        next: FeedSnapshot,
    },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct PageRecord {
    pub(crate) invalidation: ScanInvalidation,
    pub(crate) completed_submissions: Vec<SubmissionDay>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct SubmissionView<'a> {
    pub(crate) papers: &'a [PaperKey],
    pub(crate) complete: bool,
}

pub(crate) fn project_recent(cx: &Cx<State>, category: CategoryKey) -> Result<Projection> {
    cx.state(|state| {
        let mut p = Projection::new();
        p.dir("_fetched");
        p.dir("pages");
        if let Some(index) = state.recent.get(&category) {
            p.file_with_content("_status.json", index.recent_status_json()?);
            index.write_scan_page_status(&mut p);
        } else {
            p.page(PageStatus::More(Cursor::Opaque("pages/0".to_string())));
        }
        Ok(p)
    })
}

pub(crate) fn project_fetched(cx: &Cx<State>, category: CategoryKey) -> Result<Projection> {
    cx.state(|state| {
        Ok(state.recent.get(&category).map_or_else(
            || {
                let mut p = Projection::new();
                p.page(PageStatus::More(Cursor::Opaque("pages/0".to_string())));
                p
            },
            |index| index.project_fetched(&category),
        ))
    })
}

pub(crate) fn project_recent_pages(cx: &Cx<State>, category: CategoryKey) -> Result<Projection> {
    cx.state(|state| {
        let p = if let Some(index) = state.recent.get(&category) {
            index.project_recent_pages()
        } else {
            let mut p = Projection::new();
            p.dir(RecentPage::zero().to_string());
            p.page(PageStatus::More(Cursor::Opaque("pages/0".to_string())));
            p
        };
        Ok(p)
    })
}

pub(crate) async fn project_recent_page(
    cx: &Cx<State>,
    category: CategoryKey,
    page: RecentPage,
) -> Result<Projection> {
    ensure_page(cx, category.clone(), page).await?;
    cx.state(|state| {
        let index = state
            .recent
            .get(&category)
            .ok_or_else(|| ProviderError::internal("recent page was not recorded"))?;
        index.project_recent_page(&category, page)
    })
}

pub(crate) fn project_submissions(cx: &Cx<State>, category: CategoryKey) -> Result<Projection> {
    cx.state(|state| {
        Ok(state.recent.get(&category).map_or_else(
            || {
                let mut p = Projection::new();
                p.page(PageStatus::More(Cursor::Opaque(
                    "recent/pages/0".to_string(),
                )));
                p
            },
            RecentIndex::project_submissions,
        ))
    })
}

pub(crate) fn project_submission(
    cx: &Cx<State>,
    category: CategoryKey,
    day: SubmissionDay,
) -> Result<Projection> {
    cx.state(|state| {
        let index = state
            .recent
            .get(&category)
            .ok_or_else(|| ProviderError::not_found("submission day has not been discovered"))?;
        index.project_submission(&category, day)
    })
}

async fn ensure_page(cx: &Cx<State>, category: CategoryKey, page: RecentPage) -> Result<()> {
    let already_fetched = cx.state(|state| {
        state
            .recent
            .get(&category)
            .is_some_and(|index| index.recent_page(page).is_some())
    });
    if already_fetched {
        return Ok(());
    }

    // The provider state records scan bookkeeping only. The HTTP call is outside
    // `state_mut`, then the parsed page is merged in one short synchronous step.
    let fetched = fetch_category_page(cx, &category, page).await?;
    cx.state_mut(|state| {
        state
            .recent
            .entry(category)
            .or_insert_with(RecentIndex::new)
            .record_page(fetched)
    });
    Ok(())
}

impl RecentIndex {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn evaluate_invalidations(&self, page: &CategoryPage) -> ScanInvalidation {
        match self.feed_updated {
            Some(previous) if previous.utc_date() != page.snapshot.utc_date() => {
                ScanInvalidation::NewSnapshot {
                    previous,
                    next: page.snapshot,
                }
            },
            _ => ScanInvalidation::None,
        }
    }

    pub(crate) fn record_page(&mut self, page: CategoryPage) -> PageRecord {
        let invalidation = self.evaluate_invalidations(&page);
        if matches!(invalidation, ScanInvalidation::NewSnapshot { .. }) {
            *self = Self::new();
        }
        self.feed_updated = Some(page.snapshot);
        self.total_results = page.total_results;

        let before_complete = self.complete_days();
        let mut page_keys = Vec::new();
        let mut page_seen = HashSet::new();
        for paper in page.papers {
            self.record_paper(&mut page_keys, &mut page_seen, paper);
        }
        self.pages.insert(page.page, page_keys);
        self.advance_contiguous_prefix();
        self.recompute_bucket_completion();

        let completed_submissions = self
            .complete_days()
            .into_iter()
            .filter(|day| !before_complete.contains(day))
            .collect();
        self.prune_entries();

        PageRecord {
            invalidation,
            completed_submissions,
        }
    }

    pub(crate) fn fetched_pages(&self) -> Vec<RecentPage> {
        self.pages.keys().copied().collect()
    }

    pub(crate) fn recent_page(&self, page: RecentPage) -> Option<&[PaperKey]> {
        self.pages.get(&page).map(Vec::as_slice)
    }

    pub(crate) fn scan_exhausted(&self) -> bool {
        self.contiguous_through.is_some_and(|page| {
            (page.index() + 1) * u64::from(crate::types::PAGE_SIZE) >= u64::from(self.total_results)
        })
    }

    pub(crate) fn submission(&self, day: SubmissionDay) -> Option<SubmissionView<'_>> {
        self.buckets.get(&day).map(|bucket| SubmissionView {
            papers: &bucket.papers,
            complete: bucket.complete,
        })
    }

    pub(crate) fn entry(&self, key: &PaperKey) -> Option<ParsedEntry> {
        self.entries.get(key).cloned()
    }

    pub(crate) fn discovered_days(&self) -> Vec<SubmissionDay> {
        self.buckets.keys().rev().copied().collect()
    }

    fn record_paper(
        &mut self,
        page_keys: &mut Vec<PaperKey>,
        page_seen: &mut HashSet<PaperKey>,
        paper: PagePaper,
    ) {
        let key = paper.key;
        if page_seen.insert(key.clone()) {
            page_keys.push(key.clone());
        }

        let bucket = self.buckets.entry(paper.submission).or_default();
        if !bucket.papers.contains(&key) {
            bucket.papers.push(key.clone());
        }

        self.entries.entry(key).or_insert(paper.entry);
    }

    fn advance_contiguous_prefix(&mut self) {
        let mut next = self
            .contiguous_through
            .map_or(RecentPage::zero(), RecentPage::next);
        while self.pages.contains_key(&next) {
            self.include_contiguous_page(next);
            self.contiguous_through = Some(next);
            next = next.next();
        }
    }

    fn include_contiguous_page(&mut self, page: RecentPage) {
        let Some(keys) = self.pages.get(&page) else {
            return;
        };
        for key in keys {
            let Some(entry) = self.entries.get(key) else {
                continue;
            };
            let Ok(submission) = SubmissionDay::from_published(&entry.published) else {
                continue;
            };
            self.oldest_contiguous_submission = Some(
                self.oldest_contiguous_submission
                    .map_or(submission, |oldest| oldest.min(submission)),
            );
        }
    }

    fn recompute_bucket_completion(&mut self) {
        if let Some(oldest) = self.oldest_contiguous_submission {
            for (day, bucket) in &mut self.buckets {
                if *day > oldest {
                    bucket.complete = true;
                }
            }
        }

        if self.scan_exhausted() {
            for bucket in self.buckets.values_mut() {
                bucket.complete = true;
            }
        }
    }

    fn prune_entries(&mut self) {
        let mut removed_keys = HashSet::new();
        while self.pages.len() > MAX_STORED_RECENT_PAGES {
            let Some(page) = self.pages.keys().next().copied() else {
                break;
            };
            if let Some(keys) = self.pages.remove(&page) {
                removed_keys.extend(keys);
            }
        }

        if !removed_keys.is_empty() {
            let retained_page_keys: HashSet<&PaperKey> = self.pages.values().flatten().collect();
            self.buckets.retain(|_, bucket| {
                if !bucket.complete {
                    bucket.papers.retain(|key| {
                        !removed_keys.contains(key) || retained_page_keys.contains(key)
                    });
                }
                bucket.complete || !bucket.papers.is_empty()
            });
        }

        let retained_entries: HashSet<&PaperKey> = self.pages.values().flatten().collect();
        self.entries.retain(|key, _| retained_entries.contains(key));
    }

    fn project_recent_pages(&self) -> Projection {
        let mut p = Projection::new();
        for page in self.fetched_pages() {
            p.dir(page.to_string());
        }
        self.write_recent_pages_status(&mut p);
        p
    }

    fn project_submissions(&self) -> Projection {
        let mut p = Projection::new();
        for day in self.discovered_days() {
            p.dir(day.path_segment());
        }
        self.write_scan_page_status(&mut p);
        p
    }

    fn project_fetched(&self, category: &CategoryKey) -> Projection {
        let mut p = Projection::new();
        for key in self.fetched_papers() {
            p.dir(key.to_string());
            p.proj_dir(format!("categories/{category}/recent/_fetched/{key}"));
        }
        self.write_scan_page_status(&mut p);
        p
    }

    fn project_recent_page(&self, category: &CategoryKey, page: RecentPage) -> Result<Projection> {
        self.project_page(
            category,
            page,
            &format!("categories/{category}/recent/pages/{page}"),
        )
    }

    fn project_submission(&self, category: &CategoryKey, day: SubmissionDay) -> Result<Projection> {
        let view = self
            .submission(day)
            .ok_or_else(|| ProviderError::not_found("submission day has not been discovered"))?;
        let mut p = Projection::new();
        for key in view.papers {
            p.dir(key.to_string());
            let base = format!("categories/{category}/submissions/{day}/{key}");
            p.proj_dir(base);
        }
        p.file_with_content(
            "_status.json",
            self.submission_status_json(day, view.complete)?,
        );
        if view.complete {
            p.page(PageStatus::Exhaustive);
        } else {
            let cursor = self
                .next_page_path()
                .unwrap_or_else(|| "pages/0".to_string());
            p.page(PageStatus::More(Cursor::Opaque(cursor)));
        }
        Ok(p)
    }

    fn project_page(
        &self,
        category: &CategoryKey,
        page: RecentPage,
        page_prefix: &str,
    ) -> Result<Projection> {
        let keys = self
            .recent_page(page)
            .ok_or_else(|| ProviderError::not_found("recent page has not been fetched"))?;
        let mut p = Projection::new();
        for key in keys {
            p.dir(key.to_string());
            let entry = self
                .entries
                .get(key)
                .ok_or_else(|| ProviderError::internal("recent page referenced missing paper"))?;
            let page_base = format!("{page_prefix}/{key}");
            p.proj_dir(page_base);
            p.proj_dir(format!("categories/{category}/recent/_fetched/{key}"));
            let submission = SubmissionDay::from_published(&entry.published)?;
            let submission_base = format!("categories/{category}/submissions/{submission}");
            p.proj_dir(&submission_base);
            p.proj_dir(format!("{submission_base}/{key}"));
        }
        self.write_scan_page_status(&mut p);
        Ok(p)
    }

    fn recent_status_json(&self) -> Result<Vec<u8>> {
        let snapshot = self
            .feed_updated
            .ok_or_else(|| ProviderError::internal("recent status missing snapshot"))?;
        Ok(pretty_json(&json!({
            "feed_updated": snapshot.as_utc_string(),
            "total_results": self.total_results,
            "fetched_pages": self.fetched_pages().into_iter().map(RecentPage::index).collect::<Vec<_>>(),
            "next_page": self.next_page_path(),
            "last_fetch_error": null,
        })))
    }

    fn submission_status_json(&self, day: SubmissionDay, complete: bool) -> Result<Vec<u8>> {
        let snapshot = self
            .feed_updated
            .ok_or_else(|| ProviderError::internal("submission status missing snapshot"))?;
        Ok(pretty_json(&json!({
            "submission": day.path_segment(),
            "date_semantics": "utc_published_date",
            "status": if complete { "complete" } else { "partial" },
            "feed_updated": snapshot.as_utc_string(),
            "fetched_pages": self.fetched_pages().into_iter().map(RecentPage::index).collect::<Vec<_>>(),
            "next_page": if complete {
                None
            } else {
                self.next_page_path()
                    .map(|path| format!("../../recent/{path}"))
            },
        })))
    }

    fn next_page_path(&self) -> Option<String> {
        (!self.scan_exhausted()).then(|| format!("pages/{}", self.next_page_number()))
    }

    fn next_page_number(&self) -> RecentPage {
        self.contiguous_through
            .map_or(RecentPage::zero(), RecentPage::next)
    }

    fn fetched_papers(&self) -> Vec<&PaperKey> {
        let mut seen = HashSet::new();
        let mut papers = Vec::new();
        for key in self.pages.values().flatten() {
            if seen.insert(key) {
                papers.push(key);
            }
        }
        papers
    }

    fn write_scan_page_status(&self, p: &mut Projection) {
        if let Some(next_page) = self.next_page_path() {
            p.page(PageStatus::More(Cursor::Opaque(next_page)));
        } else {
            p.page(PageStatus::Exhaustive);
        }
    }

    fn write_recent_pages_status(&self, p: &mut Projection) {
        if let Some(next_page) = self.next_page_path() {
            p.dir(self.next_page_number().to_string());
            p.page(PageStatus::More(Cursor::Opaque(next_page)));
        } else {
            p.page(PageStatus::Exhaustive);
        }
    }

    fn complete_days(&self) -> Vec<SubmissionDay> {
        self.buckets
            .iter()
            .filter_map(|(day, bucket)| bucket.complete.then_some(*day))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(day: u8) -> FeedSnapshot {
        FeedSnapshot::parse(&format!("2026-05-{day:02}T00:00:00Z")).unwrap()
    }

    fn paper(raw_id: &str, published_day: u8) -> PagePaper {
        PagePaper::from_entry(ParsedEntry {
            raw_id: raw_id.to_string(),
            latest_version: 1,
            published: format!("2026-05-{published_day:02}T12:00:00Z"),
            updated: format!("2026-05-{published_day:02}T13:00:00Z"),
            title: raw_id.to_string(),
            abstract_text: String::new(),
            authors: Vec::new(),
            primary_category: None,
            categories: Vec::new(),
            dois: Vec::new(),
            journal_refs: Vec::new(),
            comments: Vec::new(),
        })
        .unwrap()
    }

    fn page(index: u32, total_results: u32, papers: Vec<PagePaper>) -> CategoryPage {
        CategoryPage {
            page: RecentPage::new(u64::from(index)),
            snapshot: snapshot(12),
            total_results,
            papers,
        }
    }

    #[test]
    fn page_zero_starts_partial_submission_bucket() {
        let mut index = RecentIndex::new();
        index.record_page(page(0, 200, vec![paper("2605.00001", 12)]));
        let day = SubmissionDay::parse_path("20260512").unwrap();
        let view = index.submission(day).unwrap();
        assert!(!view.complete);
        assert_eq!(view.papers.len(), 1);
        assert_eq!(index.contiguous_through, Some(RecentPage::zero()));
    }

    #[test]
    fn same_day_entries_can_span_multiple_pages() {
        let mut index = RecentIndex::new();
        index.record_page(page(0, 300, vec![paper("2605.00001", 12)]));
        index.record_page(page(1, 300, vec![paper("2605.00002", 12)]));
        let day = SubmissionDay::parse_path("20260512").unwrap();
        let view = index.submission(day).unwrap();
        assert!(!view.complete);
        assert_eq!(view.papers.len(), 2);
        assert_eq!(index.contiguous_through, Some(RecentPage::new(1)));
    }

    #[test]
    fn new_feed_snapshot_resets_category_scan() {
        let mut index = RecentIndex::new();
        index.record_page(page(0, 200, vec![paper("2605.00001", 12)]));
        let changed = CategoryPage {
            page: RecentPage::zero(),
            snapshot: snapshot(13),
            total_results: 1,
            papers: vec![paper("2605.00002", 13)],
        };
        let record = index.record_page(changed);
        assert!(matches!(
            record.invalidation,
            ScanInvalidation::NewSnapshot { .. }
        ));
        assert!(
            index
                .submission(SubmissionDay::parse_path("20260512").unwrap())
                .is_none()
        );
        assert!(
            index
                .submission(SubmissionDay::parse_path("20260513").unwrap())
                .is_some()
        );
    }

    #[test]
    fn same_day_feed_timestamp_change_keeps_category_scan() {
        let mut index = RecentIndex::new();
        index.record_page(page(0, 200, vec![paper("2605.00001", 12)]));
        let changed = CategoryPage {
            page: RecentPage::new(1),
            snapshot: FeedSnapshot::parse("2026-05-12T00:00:01Z").unwrap(),
            total_results: 200,
            papers: vec![paper("2605.00002", 12)],
        };
        let record = index.record_page(changed);

        assert_eq!(record.invalidation, ScanInvalidation::None);
        assert_eq!(index.contiguous_through, Some(RecentPage::new(1)));
    }

    #[test]
    fn overlapping_page_entries_keep_page_view_and_dedupe_indexes() {
        let mut index = RecentIndex::new();
        index.record_page(page(0, 300, vec![paper("2605.00001", 12)]));
        index.record_page(page(
            1,
            300,
            vec![paper("2605.00001", 12), paper("2605.00002", 12)],
        ));
        let day = SubmissionDay::parse_path("20260512").unwrap();
        let view = index.submission(day).unwrap();
        assert_eq!(view.papers.len(), 2);
        assert_eq!(index.recent_page(RecentPage::new(1)).unwrap().len(), 2);
        assert_eq!(index.fetched_papers().len(), 2);
    }

    #[test]
    fn recent_index_retains_only_the_recent_page_window_for_partial_buckets() {
        let mut index = RecentIndex::new();
        for page_index in 0..25 {
            index.record_page(page(
                page_index,
                10_000,
                vec![paper(&format!("2605.{page_index:05}"), 12)],
            ));
        }
        let day = SubmissionDay::parse_path("20260512").unwrap();

        assert_eq!(index.pages.len(), MAX_STORED_RECENT_PAGES);
        assert_eq!(index.entries.len(), MAX_STORED_RECENT_PAGES);
        assert_eq!(
            index.submission(day).unwrap().papers.len(),
            MAX_STORED_RECENT_PAGES
        );
        assert!(index.recent_page(RecentPage::new(0)).is_none());
        assert!(index.recent_page(RecentPage::new(24)).is_some());
    }

    #[test]
    fn undiscovered_submission_returns_not_found() {
        let index = RecentIndex::new();
        let day = SubmissionDay::parse_path("20260512").unwrap();
        let category = "cs.AI".parse().unwrap();
        assert!(index.project_submission(&category, day).is_err());
    }
}
