# omnifs github mount: agent guide

This mount projects GitHub into a filesystem. Read this file first; it
describes the shape, the cheap reads, and the queries you can run.

## Top-level shape

```
<mount>/
  AGENT.md              this file
  .events               NDJSON tail of recent provider events (poll-readable)
  <owner>/              owner or org; lazily resolved on first access
    <repo>/             repo root
      _repo/            git tree of the default branch (subtree handoff)
      _issues/
        _open/          open issues, paginated
        _all/           all issues (open and closed)
        */<number>/     issue detail; siblings: title, body, state, user, summary.md, comments/
      _prs/             same shape as _issues, but for pull requests
      _q/               saved-search-style query views
        issues/<query>/ issues matching `<query>` (see "Query syntax")
        prs/<query>/    PRs matching `<query>`
      _actions/runs/    workflow runs
```

## How to read efficiently

Every numbered resource (issue, PR) projects a `summary.md` sibling with
the title, state, author, and a body excerpt bundled into one read.
Prefer `cat .../summary.md` over four separate `cat`s of `title`,
`body`, `state`, `user` when you just need orientation.

When you list a directory of issues or PRs, the host already has the
common fields and `summary.md` cached for every numbered child. Stat-ing
or reading those siblings does not trigger another API round trip.

## Query syntax (`_q/issues/<query>` and `_q/prs/<query>`)

The single path segment after `_q/issues/` or `_q/prs/` is appended
directly to GitHub's Search API `q=` qualifier (after a
`repo:<owner>/<repo>` prefix the provider adds, plus `is:issue` or
`is:pr`). Use `+` between qualifiers; `:` separates a qualifier from
its value.

Examples:

- `_q/issues/state:open+author:raulk` — open issues authored by raulk
- `_q/issues/state:open+label:bug` — open issues labelled bug
- `_q/issues/is:closed+mentions:raulk` — closed issues mentioning raulk
- `_q/prs/is:merged+author:raulk` — PRs merged by raulk

Each result is a numbered directory with the same siblings as the
fixed-filter listings. Capped at one Search page (100 results); the
listing is marked non-exhaustive when the search has more matches.
The `&`, `#`, and `/` characters are unsafe in queries — `&` and `#`
break the URL, `/` splits the path segment.

## Live updates (`.events`)

The provider polls active repositories on a timer and records what it
saw to the in-memory event log surfaced at `<mount>/.events`. Each
poll appends a single NDJSON line. Useful in agent loops:

```sh
while true; do
  tail -n 20 <mount>/.events
  sleep 5
done
```

The file is invalidated on every event, so each poll returns fresh
content.

## What is *not* projected

- Random user/org enumeration — `ls <mount>` is intentionally empty
  apart from `AGENT.md` and `.events`. Navigate by typing a known
  owner/repo path.
- Writes — `_issues` is read-only in this mount. Future work plugs
  writes through the reconcile surface.
