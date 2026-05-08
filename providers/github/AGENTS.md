# GitHub provider — local rules

Conventions and gotchas specific to `providers/github/`. For shared
provider rules (WIT, dispatch, callouts, configs), see
`.rules/provider-sdk.md` at the repo root.

## Auth split

- **API calls** use the bearer token. Capability declares
  `domains: ["api.github.com"]`, `auth_types: ["bearer-token"]`
  (`provider.rs:30`).
- **Git clones** use SSH (`needs_git: true`, `provider.rs:33`). The
  cache-key + clone-url contract is in `repo.rs:23-24` —
  `github.com/{owner}/{repo}` and `git@github.com:{owner}/{repo}.git`.
  This shape is a contract with the host clone manager; don't change it
  without a coordinated host-side update.

Don't mix the two: bearer tokens never reach `cx.git()`, and SSH never
reaches the API surface.

## Required API headers, every call

Every JSON request goes through `cx.github_*` helpers in `http_ext.rs`,
which set:

- `X-GitHub-Api-Version: 2022-11-28`
- `Accept: application/vnd.github+json`

For special payloads (PR diff, etc.), override **only** the `Accept`
header (e.g. `application/vnd.github.diff`); the version header is still
required (`http_ext.rs:26, 31`).

## All API responses go through `github_check_status`

`github_check_status` (`http_ext.rs:48-70`) maps GitHub-flavored 403s
into `ProviderError::RateLimited` (retryable) when:

- `x-ratelimit-remaining: 0`, or
- the body matches "rate limit" / "abuse detection".

If you call the GitHub API and skip this helper, rate-limit hits surface
as generic 403s and the caller can't distinguish them from real
permission errors. Always route through it.

## Hybrid issue/PR pagination

Issue and PR listings split work across two GitHub endpoints
(`numbered.rs:75-120`):

- **Page 1**: `search/issues` — gives `total_count` so the SDK can
  return a sized listing immediately.
- **Pages 2..N**: `/repos/{owner}/{repo}/{resource}` — fetched in
  parallel; cheaper per-call and avoids the search API's quirks.

Two consequences:

1. **Search API is hard-capped at 1000 results**
   (`SEARCH_RESULT_CAP = 1000`). When `total_count > 1000`, set
   `exhaustive: false` and surface an opaque cursor — listings beyond
   item 1000 are not retrievable through this path.
2. **Dedupe across the seam.** Items can be created/deleted between the
   page-1 and page-2 calls, shifting offsets; the merge step dedupes by
   ID. Don't remove the dedupe.

## PR cross-listing preload

The issues endpoint returns PRs too — each item carries a `pull_request`
field (`issues.rs:17`, `issues.rs:87`). When the issue list handler sees
that field set, it skips the issues tree and **preloads the PR's
title/body/state/user under `_prs/{filter}/{n}/`**, plus the `diff` file
and `comments/` directory.

This is the load-bearing reason browsing `_issues/` is "free" for
populating `_prs/`: don't drop the cross-listing during refactors. If
you change the issues list shape, run the demo against a repo with
both issues and PRs and verify both subtrees populate.

## Filter values

`StateFilter` has exactly two variants (`types.rs:6-13`):

- `_open`
- `_all`

There is **no `_closed`**. If you find yourself wanting to filter for
closed-only, build it on top of `_all` rather than introducing a third
filter — adding one shifts URL space that's already user-visible.

## Comment indexing

Comments are accessed by 1-indexed integer name (`numbered.rs:156-157`):

- `(page, offset_in_page) = ((idx - 1) / 100 + 1, (idx - 1) % 100)`
- `COMMENT_PAGE_SIZE = 100` (`numbered.rs:11`).
- Index `0` is "not_found" — the filesystem indexing starts at 1.

**Known gap:** the comments-list path currently fetches only page 1 and
returns an opaque "more" cursor when full (`numbered.rs:182-187`). The
multi-page list isn't wired through yet. Lookup-by-index works for
indexes beyond page 1; only `readdir` of the comments directory is
capped.

## Owner kind detection

`resolve_owner_kind` (`owners.rs:67-94`):

1. Hit `/users/{owner}` first. GitHub returns `type: "Organization"`
   for org names through this endpoint, so most callers terminate here.
2. Fall back to `/orgs/{owner}` **only** if `/users/` returns 404. Some
   org names don't resolve through `/users/`; this is the catch.
3. 404 from both is a real not-found; don't add a third lookup.

Reordering the calls (orgs first) breaks the common case — the user
endpoint is the canonical answer for both kinds.

## Per-repo event ETags

`State.event_etags: HashMap<RepoId, String>` (`lib.rs:48`,
`provider.rs:18`). The 60-second timer tick (`refresh_interval_secs: 60`,
`provider.rs:36`) walks active repos and:

1. Reads the cached ETag for the repo (`events.rs:36`).
2. Sends `If-None-Match: <etag>` (`events.rs:41`).
3. On 200, processes events and stores the new ETag (`events.rs:94`).
4. On 304, skips processing and leaves the cache as-is.

If you add a new event surface, follow the same pattern: ETag in,
ETag out, no time-based expiration. Don't memoize anything else on
`State` — the caching guidance in `.rules/caching.md` still applies.

## No root `/` handler

There's no GitHub API to enumerate visible owners across the whole
service, so the provider has no `/` `#[dir]` handler (`root.rs:11-12`).
The SDK's auto-navigable rule covers `/` from the `/{owner}` routes —
don't write a stub handler for it.

## Sibling/preload projection on every payload you touch

Generic SDK rule, restated because it's load-bearing for this provider:
when you fetch an issue, PR, or actions run, the response body already
contains `title`, `body`, `state`, `user`, etc. Project them as sibling
files / preload entries so subsequent file reads skip the API.

See `issues.rs:93-96` and `actions.rs:37-42` for the canonical pattern.
