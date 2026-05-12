# arXiv recent submissions redesign

Status: proposed
Scope: `providers/arxiv`

## Problem

The current arXiv provider treats category day paths as live arXiv API
queries:

```text
search_query=cat:{category} AND submittedDate:[YYYYMMDD0000 TO YYYYMMDD2359]
```

Those `submittedDate` range queries are extremely slow in live use, even
with small `max_results` values, and they can push the client into 429
responses. They are also the wrong model for arXiv's API lifecycle. arXiv
publishes API results on a daily cycle; a feed's `<updated>` value identifies
the API snapshot, and repeated requests for the same query within that snapshot
should be cached.

The provider should stop using `submittedDate` as a query term entirely. It
should fetch small recent category pages, materialize paper nodes from those
pages, and file discovered papers into immutable submission-day directories.

## Target model

The provider has one live moving view per category:

```text
/categories/{category}/recent
/categories/{category}/recent/_fetched
/categories/{category}/recent/pages
/categories/{category}/recent/pages/{n}
```

It also has immutable submission buckets populated from the recent scan:

```text
/categories/{category}/submissions
/categories/{category}/submissions/{YYYYMMDD}
```

Direct paper access remains available:

```text
/papers/{paper}
```

`recent` is a control namespace. `recent/pages/{n}` is the explicit fetch-next
interaction and upstream page view. `recent/_fetched` is the deduped set of
papers discovered so far by traversing fetched pages; it is non-exhaustive
until the scan exhausts. `submissions/{YYYYMMDD}` is never backed by a
`submittedDate` query; it is a materialized bucket derived from already fetched
recent pages.

## API contract

The only live listing query for category traversal is:

```text
https://export.arxiv.org/api/query?search_query=cat:{category}&start={n*100}&max_results=100&sortBy=submittedDate&sortOrder=descending
```

`sortBy=submittedDate` orders by the original submission timestamp, which
corresponds to each Atom entry's `<published>` timestamp. The provider derives
`submissions/YYYYMMDD` from the UTC date of `<published>`. This date is an
arXiv API/Atom published-date bucket, not a local wall-clock date and not a
claim about the human announcement cutoff.

The provider must parse feed-level `<updated>` and treat it as the recent scan
snapshot id. If a fetched page returns a different `<updated>` from the
category's in-progress recent scan, the provider resets that category's recent
scan and starts over from page zero. Completed submission buckets from previous
scans remain valid only once they have been marked complete.

## State and interface sketch

Provider state owns scan bookkeeping, not content caching. Projected bytes and
paper subtree content continue to flow through host browse caches and preloads.

```rust
pub struct State {
    pub config: Config,
    pub recent: HashMap<CategoryKey, RecentIndex>,
}

struct RecentIndex {
    feed_updated: OffsetDateTime,
    total_results: u32,
    pages: BTreeMap<u32, Vec<PaperKey>>,
    entries: HashMap<PaperKey, ParsedEntry>,
    buckets: HashMap<SubmissionDay, BucketState>,
    contiguous_through: Option<u32>,
}

struct BucketState {
    papers: Vec<PaperKey>,
    complete: bool,
}
```

The target Rust interface should be small and explicit. The exact names can
change during implementation, but the shape should stay close to this:

```rust
pub(crate) const PAGE_SIZE: u32 = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FeedSnapshot(OffsetDateTime);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct RecentPage(u32);

impl RecentPage {
    pub(crate) fn new(index: u32) -> Result<Self>;
    pub(crate) fn index(self) -> u32;
    pub(crate) fn start(self) -> u32; // index * PAGE_SIZE
    pub(crate) fn next(self) -> Self;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct SubmissionDay(Date);

impl SubmissionDay {
    pub(crate) fn parse_path(segment: &str) -> Result<Self>;
    pub(crate) fn from_published(published: OffsetDateTime) -> Self;
    pub(crate) fn date(self) -> Date;
    pub(crate) fn path_segment(self) -> String; // YYYYMMDD
}

pub(crate) struct CategoryPage {
    pub(crate) page: RecentPage,
    pub(crate) snapshot: FeedSnapshot,
    pub(crate) total_results: u32,
    pub(crate) papers: Vec<PagePaper>,
}

pub(crate) struct PagePaper {
    pub(crate) key: PaperKey,
    pub(crate) submission: SubmissionDay,
    pub(crate) entry: ParsedEntry,
}
```

The HTTP/parser boundary should expose one category page operation:

```rust
pub(crate) async fn fetch_category_page(
    cx: &Cx<State>,
    category: &CategoryKey,
    page: RecentPage,
) -> Result<CategoryPage>;
```

The recent-scan module should expose projection functions that map directly to
route handlers:

```rust
pub(crate) fn project_recent(
    cx: &Cx<State>,
    category: CategoryKey,
) -> Result<Projection>;

pub(crate) fn project_fetched(
    cx: &Cx<State>,
    category: CategoryKey,
) -> Result<Projection>;

pub(crate) fn project_recent_pages(
    cx: &Cx<State>,
    category: CategoryKey,
) -> Result<Projection>;

pub(crate) async fn project_recent_page(
    cx: &Cx<State>,
    category: CategoryKey,
    page: RecentPage,
) -> Result<Projection>;

pub(crate) fn project_submissions(
    cx: &Cx<State>,
    category: CategoryKey,
) -> Result<Projection>;

pub(crate) fn project_submission(
    cx: &Cx<State>,
    category: CategoryKey,
    day: SubmissionDay,
) -> Result<Projection>;
```

The pure scan-state type should be directly testable:

```rust
impl RecentIndex {
    pub(crate) fn evaluate_invalidations(&self, page: &CategoryPage) -> ScanInvalidation;
    pub(crate) fn record_page(&mut self, page: CategoryPage) -> PageRecord;
    pub(crate) fn fetched_pages(&self) -> Vec<RecentPage>;
    pub(crate) fn recent_page(&self, page: RecentPage) -> Option<&[PaperKey]>;
    pub(crate) fn submission(&self, day: SubmissionDay) -> Option<SubmissionView<'_>>;
    pub(crate) fn discovered_days(&self) -> Vec<SubmissionDay>;
}

pub(crate) enum ScanInvalidation {
    None,
    NewSnapshot { previous: FeedSnapshot, next: FeedSnapshot },
}

pub(crate) struct PageRecord {
    pub(crate) invalidation: ScanInvalidation,
    pub(crate) completed_submissions: Vec<SubmissionDay>,
}

pub(crate) struct SubmissionView<'a> {
    pub(crate) papers: &'a [PaperKey],
    pub(crate) complete: bool,
}
```

The implementation must not hold a `state_mut` borrow across `.await`. Handlers
copy the request state they need, perform the HTTP call, then write the parsed
result back into state.

The index should dedupe papers by raw arXiv id within a category and
`feed_updated` scan. ArXiv does not promise deterministic ordering for entries
with identical submitted timestamps, so adjacent pages can overlap or shuffle
near page boundaries.

The in-memory index can be bounded in the first implementation. A practical
bound is the last N fetched pages per category plus completed buckets reached
by that scan. Losing the index on provider restart is acceptable for the first
version; users can repopulate it by listing recent pages again.

## Engineering quality bar

This redesign should remove the old abstraction shape rather than adapting it.
The goal is not to preserve a generic arXiv query framework; the goal is a
small, direct provider whose code makes the supported filesystem model obvious.

This is a cleanup task, not only a behavior change. Cleanup work must be done
as obsolete code becomes visible. Do not defer removal of dead helpers, legacy
route scaffolding, compatibility shims, or awkward transitional types to a later
PR. Each implementation step should leave the touched area in its intended
final shape before moving on.

The final code should read as if it was written around one domain operation:
scan recent category pages and materialize paper nodes into recent and
submission-day views. Route handlers should be thin and declarative. Query
construction should have one narrow entrypoint. Feed parsing and scan-state
updates should be pure enough to test without FUSE or HTTP.

Prefer typed values at the model boundary:

- `SubmissionDay` for `YYYYMMDD`.
- `RecentPage` or a bounded page index for `start = n * 100`.
- A parsed feed snapshot type carrying `feed_updated`, `total_results`, and
  entries.
- A scan-state type that owns bucket completion and dedupe.

Avoid string-bag APIs and transitional bridge layers. Removing `/authors`,
`/search`, `updated`, `by-author`, and date-query routes should also remove the
types and helpers that only existed to support them. Do not keep generic
selector plumbing, `SortAxis`, compound query builders, or compatibility names
unless a remaining route still needs them.

Comments should explain load-bearing invariants, not restate code. Good
comments here include: why `sortBy=submittedDate` pairs with `<published>`, why
`state_mut` borrows must not cross `.await`, why provider state is scan
bookkeeping rather than content caching, and why partial submission buckets must
project as non-exhaustive.

Do not leave TODOs or comments that apologize for an unfinished shape. If a
temporary bridge is needed while moving code, remove it before the branch is
considered complete.

## Intended module layout

The implementation should make ownership boundaries visible in the file layout.
It does not need a file per concept. Start consolidated and split only if the
implementation becomes difficult to read. The intended shape is:

```text
providers/arxiv/src/
  api.rs          # HTTP fetches, category page fetch, Atom feed parsing
  categories.rs   # route declarations and thin handler glue
  paper.rs        # PaperSubtree, /papers binding, metadata, PDF/source reads
  provider.rs     # provider init and route registration
  recent.rs       # scan state, completion rules, recent/submissions projection
  root.rs         # root comments or explicit root projection if needed
  types.rs        # path/domain newtypes that remain useful
```

`categories.rs` should not accumulate query construction, feed parsing,
dedupe, bucket completion, or JSON status assembly. It should parse route
captures, call into `recent.rs`, and bind paper subtrees. If a handler needs
more than a small amount of local glue, that logic belongs in `recent.rs`.

`recent.rs` owns both the provider-facing orchestration and the pure scan-state
rules. Keep those as separate structs/functions inside the file. Its tests
should drive parsed page snapshots into `RecentIndex` without HTTP, FUSE, route
macros, or provider contexts.

`api.rs` should own the one category recent-page URL helper privately if
possible. If paper resource URL helpers still need to be shared with
`paper.rs`, keep them in `paper.rs` or `types.rs`; do not keep a standalone
`query.rs` just to preserve the old module shape.

The following existing modules should disappear unless implementation proves a
real need for them:

```text
authors.rs
search.rs
selector.rs
query.rs
paper_subtree.rs
papers.rs
http_ext.rs
```

`paper_subtree.rs` and `papers.rs` are small enough to fold into `paper.rs`.
`http_ext.rs` is a single User-Agent wrapper and can be folded into `api.rs`.
`query.rs` should disappear if URL construction is reduced to category-page
fetches plus paper resource links.

## Fetch and projection

Listing `/categories/{category}/recent` does not fetch arXiv and does not mix
papers with controls. It returns `_fetched`, `pages`, and a `_status.json` once
the category has scan state.

Listing `/categories/{category}/recent/_fetched` enumerates the deduped paper
nodes discovered by fetched recent pages. It grows as users traverse
`recent/pages/{n}`. It is non-exhaustive until the contiguous scan exhausts
`total_results`.

Listing `/categories/{category}/recent/pages/{n}` fetches or reuses page `n`,
where `start = n * 100`, and returns that page's paper nodes. The page listing
also records every returned paper in provider state and preloads the paper
subtree under all derived paths:

```text
/categories/{category}/recent/_fetched/{paper}
/categories/{category}/recent/pages/{n}/{paper}
/categories/{category}/submissions/{YYYYMMDD}/{paper}
```

The semantic homes for discovered category papers are `_fetched` and their
submission buckets. Page paths are the explicit fetch-next mechanism and a
useful view of the upstream API page.

Listing `/categories/{category}/submissions` enumerates submission-day
directories already discovered in state. It is non-exhaustive because more days
may appear after fetching more recent pages.

Listing `/categories/{category}/submissions/{YYYYMMDD}` reads only provider
state. It must not call arXiv. If the day has not been discovered, it returns
not found rather than an empty exhaustive directory. If the day has been
discovered but not completed, it returns a non-exhaustive listing.

## Completion

A submission bucket is complete when the contiguous fetched page prefix proves
that no more entries for that day remain.

The provider tracks the highest contiguous fetched page index beginning at
zero. A bucket `D` is complete when either condition holds:

1. The contiguous prefix contains entries for `D` and later contains an entry
   whose published UTC day is older than `D`.
2. The contiguous prefix exhausts the feed, so `(contiguous_through + 1) * 100`
   is greater than or equal to `total_results`.

Buckets observed only through a non-contiguous page fetch remain partial until
the earlier gap is filled. Partial buckets project with `PageStatus::More`;
complete buckets project with `PageStatus::Exhaustive`.

## Status file

Use a single `_status.json` control file. Do not keep the old `listing.json`
and do not add a separate `_more` marker for this surface.

For `recent` and `recent/_fetched`, status includes:

```json
{
  "feed_updated": "2026-05-12T00:00:00Z",
  "total_results": 123456,
  "fetched_pages": [0, 1],
  "next_page": "pages/2",
  "last_fetch_error": null
}
```

For `submissions/{YYYYMMDD}`, status includes:

```json
{
  "submission": "20260512",
  "date_semantics": "utc_published_date",
  "status": "partial",
  "feed_updated": "2026-05-12T00:00:00Z",
  "fetched_pages": [0],
  "next_page": "../../recent/pages/1"
}
```

Partial status files are mutable observations. Complete submission status files
can be projected as immutable once the bucket is closed.

## Scope changes

Remove the query surfaces that preserve the slow or sophisticated model:

```text
/categories/{category}/{year}/{month}/{day}
/categories/{category}/new
/categories/{category}/new/{n}
/categories/{category}/updated
/categories/{category}/updated/{n}
/categories/{category}/by-author/{author}
/authors/{author}
/search/{query}
```

This is unreleased provider work, so no compatibility shims are required. The
remaining category traversal should be built around `recent`, `recent/pages`,
and `submissions`.

Do not add a provider-side sleep loop for arXiv rate limits in this slice. The
host/runtime owns HTTP rate-limit behavior. If 429 handling remains wrong, fix
that in the host path rather than hiding it in the provider.

## Quality gates

The implementation must satisfy gates that test the shape, not only the runtime
behavior.

### Forbidden-pattern gate

Fail review if these terms remain in `providers/arxiv/src` after the redesign,
unless the occurrence is in a deletion-oriented test fixture or this document:

```text
submittedDate
SortAxis::Updated
lastUpdatedDate
category_day_query
author_query
by-author
/authors
/search
MAX_WINDOW_INDEX
```

### Single live query gate

Route or API tests must prove that every category listing call uses the single
allowed live query shape:

```text
search_query=cat:{category}
max_results=100
sortBy=submittedDate
sortOrder=descending
start={page*100}
```

No route under category traversal may produce a compound `search_query`, a
`submittedDate` range, or an `updated` sort.

### Module-boundary gate

Review the final file layout against the intended module layout above. In
particular, `categories.rs` must stay route-shaped, `recent.rs` must contain
the pure scan-state rules behind testable structs, and no old generic
`query.rs`/selector architecture should survive just because it existed before.

### No deferred cleanup gate

Fail review if the implementation leaves cleanup as future work. In
`providers/arxiv/src`, there should be no new `TODO`, `FIXME`, `legacy`,
`compat`, `shim`, or "clean up later" comments related to this redesign.
Obsolete helpers, route modules, selector plumbing, query builders, imports,
tests, and comments must be removed in the same implementation pass that makes
them unused.

### Dead-code gate

The provider should compile without dead-code allowances introduced for this
redesign. Do not keep unused functions, types, modules, imports, or test
fixtures to preserve old structure. `cargo check`, `cargo clippy`, and a manual
`rg` pass over `providers/arxiv/src` should agree that the remaining code is
live in the reduced model.

### Interface-shape gate

Public or cross-module APIs in the provider should carry domain concepts, not
raw strings and loose integers. Review the final interfaces for
`FeedSnapshot`, `RecentPage`, `SubmissionDay`, `CategoryPage`, `PagePaper`, and
`RecentIndex`. If a route or projection path passes around raw page indexes,
raw `YYYYMMDD` strings, or untyped query fragments after parsing, fix it before
moving on.

### Readability gate

The final provider should read top down: routes in `categories.rs`, orchestration
in `recent.rs`, feed fetching/parsing in `api.rs`, and paper projection in
`paper.rs`. A reviewer should not need to understand removed authors/search/date
features to follow the current category recent/submissions flow. If a file
still has mixed concerns or historical names from the deleted model, keep
cleaning before validation.

### Pure index test gate

Bucket completion, raw-id dedupe, `feed_updated` reset, scan exhaustion, and
contiguous page tracking must be covered by tests that do not instantiate HTTP,
FUSE, route macros, or provider contexts. If these tests need mocks, the design
is too tangled.

### Async state-borrow gate

No `RefCell` or `state_mut` borrow may cross `.await`. Enforce with Clippy if
available and inspect manually in review. The fetch path should copy what it
needs before awaiting and write results back afterward.

### No compatibility shim gate

Deleted surfaces must be gone, not redirected or left as aliases:

```text
/categories/{category}/{year}/{month}/{day}
/categories/{category}/updated
/categories/{category}/by-author/{author}
/authors/{author}
/search/{query}
```

### Status semantics gate

Tests must prove these three outcomes:

- partial `submissions/{YYYYMMDD}` projects `PageStatus::More`
- complete `submissions/{YYYYMMDD}` projects `PageStatus::Exhaustive`
- undiscovered `submissions/{YYYYMMDD}` returns not found

### Container smoke gate

Runtime validation must exercise the supported container path and then inspect
logs to prove that no `submittedDate` query was emitted:

```bash
ls /arxiv/categories/cs.AI/recent
ls /arxiv/categories/cs.AI/recent/_fetched
ls /arxiv/categories/cs.AI/recent/pages/1
ls /arxiv/categories/cs.AI/submissions
find /arxiv/categories/cs.AI -maxdepth 4 -type d | head
```

### External review gate

Before pushing the implementation, ask Claude for a narrow cleanup review:

```text
Review only providers/arxiv. Did the implementation preserve the reduced
model, or did it leave generic query/selector architecture behind? Call out
slop, unnecessary abstraction, and hidden old behavior.
```

## Implementation sequence

The checklist below is a coverage list, not the order of work. Implement in
this sequence so the risky logic is isolated before route churn.

1. Add the domain model: `FeedSnapshot`, `RecentPage`, `SubmissionDay(Date)`,
   `CategoryPage`, and `PagePaper`. Keep this compiling with minimal use.
2. Build the API/parser boundary: implement the single allowed category-page
   fetch, parse feed `<updated>`, and derive `SubmissionDay` from entry
   `<published>`.
3. Build the pure `RecentIndex`: page recording, raw-id dedupe,
   `feed_updated` invalidation, contiguous page tracking, bucket completion,
   and scan exhaustion. Test this before touching routes.
4. Add recent/submission projection orchestration in `recent.rs`, including
   `_status.json` generation.
5. Replace the category route surface with thin handlers for `recent`,
   `recent/pages`, `recent/pages/{n}`, `submissions`, and
   `submissions/{YYYYMMDD}`.
6. Consolidate the paper subtree by folding `paper_subtree.rs` and `papers.rs`
   into `paper.rs`, then bind paper nodes from the new category paths.
7. Delete old surfaces and dead modules: `/authors`, `/search`, date routes,
   `new`, `updated`, `by-author`, selector plumbing, and generic query helpers.
8. Run the shape gates: forbidden-pattern grep, no deferred cleanup, dead code,
   interface shape, readability, single live query tests, no compatibility
   shims, pure index tests, and module-boundary review.
9. Run full validation: Rust checks, live-container acceptance, log inspection
   proving no `submittedDate` query, and the narrow Claude cleanup review.

## Implementation checklist

- [ ] Delete `submittedDate` query construction from the arXiv provider.
- [ ] Replace `MAX_PAGE_SIZE` with `100` for arXiv listing pages.
- [ ] Replace the generic listing URL API with a category recent-page URL builder.
- [ ] Remove `SortAxis::Updated`, `category_day_query`, `author_query`, `and`,
      `window_start`, and `MAX_WINDOW_INDEX`.
- [ ] Parse feed-level `<updated>` into `CategoryPage`.
- [ ] Parse entry `<published>` into a `SubmissionDay` `YYYYMMDD` key.
- [ ] Add provider state for recent scan bookkeeping.
- [ ] Add `RecentIndex` page recording with raw-id dedupe.
- [ ] Reset a category scan when any fetched page reports a new `feed_updated`.
- [ ] Track contiguous fetched pages beginning at zero.
- [ ] Mark buckets complete when the scan crosses into an older published UTC
      day.
- [ ] Mark all open buckets complete when the contiguous scan exhausts
      `total_results`.
- [ ] Add explicit handlers for `/categories/{category}/recent`.
- [ ] Add explicit handlers for `/categories/{category}/recent/_fetched`.
- [ ] Add explicit handlers for `/categories/{category}/recent/pages`.
- [ ] Add explicit handlers for `/categories/{category}/recent/pages/{n}`.
- [ ] Add explicit handlers for `/categories/{category}/submissions`.
- [ ] Add explicit handlers for `/categories/{category}/submissions/{YYYYMMDD}`.
- [ ] Bind paper subtrees under recent `_fetched` and page paths.
- [ ] Bind paper subtrees under submission bucket paths.
- [ ] Project `_status.json` for recent and submission directories.
- [ ] Return not found for undiscovered `submissions/{YYYYMMDD}`.
- [ ] Return `PageStatus::More` for partial submission buckets.
- [ ] Return `PageStatus::Exhaustive` for complete submission buckets.
- [ ] Remove category year/month/day route handlers.
- [ ] Remove category `new`, `updated`, and `by-author` route handlers.
- [ ] Remove `authors.rs` from the provider route surface.
- [ ] Remove `search.rs` from the provider route surface.
- [ ] Fold `paper_subtree.rs` and `papers.rs` into `paper.rs`.
- [ ] Fold `http_ext.rs` into `api.rs`.
- [ ] Delete `query.rs` if its remaining URL helpers can live in `api.rs` and
      `paper.rs`.
- [ ] Update root/provider comments to describe the reduced path surface.
- [ ] Delete old selector/query abstractions that no remaining route uses.
- [ ] Move recent scan bookkeeping into a small focused module rather than
      growing `categories.rs` into a mixed route/state/query file.
- [ ] Keep route handlers thin: parse captures, call the recent-scan API, and
      project the returned model.
- [ ] Keep URL construction in one narrow helper for category recent pages.
- [ ] Keep feed parsing separate from scan-state mutation.
- [ ] Use typed `SubmissionDay` and page-index values instead of passing raw
      strings and integers through the implementation.
- [ ] Add load-bearing comments for scan snapshot invariants and async state
      borrowing, and remove comments that only describe obvious code.
- [ ] Run the forbidden-pattern gate over `providers/arxiv/src`.
- [ ] Run the no-deferred-cleanup gate over `providers/arxiv/src`.
- [ ] Run the dead-code gate over `providers/arxiv/src`.
- [ ] Review the final cross-module interfaces for typed domain concepts.
- [ ] Review readability top down from routes to orchestration to API parsing.
- [ ] Confirm the implementation matches the intended module layout.
- [ ] Confirm scan-state tests do not depend on HTTP, FUSE, route macros, or
      provider contexts.
- [ ] Confirm deleted route surfaces are gone rather than redirected.
- [ ] Ask Claude for the narrow cleanup review before pushing.
- [ ] Bound the in-memory recent index.
- [ ] Handle missing or unparsable feed `<updated>` without merging the page
      into a scan.
- [ ] Add URL-construction tests for `cat:{category}`, `max_results=100`, and
      `start=n*100`.
- [ ] Add parser tests for feed `<updated>` and entry `<published>` to
      `YYYYMMDD`.
- [ ] Add index tests for page-zero partial state.
- [ ] Add index tests for same-day entries spanning multiple pages.
- [ ] Add index tests for completion after crossing into an older day.
- [ ] Add index tests for scan exhaustion closing open buckets.
- [ ] Add index tests for feed-updated mismatch resetting the category scan.
- [ ] Add dedupe tests for overlapping page entries.
- [ ] Add route tests proving `submissions/{YYYYMMDD}` does not issue a
      `submittedDate` URL.
- [ ] Add route tests proving `recent/pages/1` issues `start=100`.
- [ ] Add route tests for unknown submission dates returning not found.
- [ ] Add smoke coverage for shell traversal over both `recent` and
      `submissions` paths.
- [ ] Run `cargo fmt --all --check`.
- [ ] Run `cargo test -p omnifs-provider-arxiv --all-targets`.
- [ ] Run `cargo check -p omnifs-provider-arxiv --target wasm32-wasip2`.
- [ ] Run `cargo clippy -p omnifs-provider-arxiv --all-targets -- -D warnings`.
- [ ] Validate the container path with `just dev` and `omnifs status`.
- [ ] Validate `/arxiv/categories/{category}/recent` and
      `/arxiv/categories/{category}/submissions/{YYYYMMDD}` inside the
      container.
