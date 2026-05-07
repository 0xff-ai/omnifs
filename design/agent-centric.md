# Agent-centric provider conventions

Status: draft, PoC in flight on `claude/omnifs-agent-centric-design-MxUZP`
Scope: `providers/github`, plus a few host-trivial primitives the SDK already
exposes; no WIT changes.

## Goal

Make omnifs the cheapest way for an LLM agent to query a service it has
mounted. Beat MCP and direct API access on three axes:

1. Tokens: every read should cost as few tokens as possible. The "what is
   this?" question gets a 200-token answer, not a 2k-token JSON dump.
2. Latency: a typical "navigate, summarize, drill in" loop should issue
   1–2 provider round trips at most, not one per file the agent stats.
3. Discoverability: the agent should learn the mount's shape by walking
   it, not by reading external docs.

The provider WIT and host runtime already give us the levers (sibling
files, preload, exhaustive listings, `notify::on_event` invalidations,
literal-vs-capture route precedence). This document covers the
provider-author conventions that exploit those levers.

## Four conventions

### 1. `AGENT.md` at the mount root

A static, hand-written file projected at `<mount>/AGENT.md`. The agent
reads this once when it first lands on the mount and gets:

- the directory shape (dynamic dirs, named dirs, projected files);
- which paths are writable (today: none; reserved for the reconcile
  surface);
- common tasks ("how do I find issues mentioning X?", "how do I tail
  the live event log?");
- the query grammar for `_issues/q/...` (see §4).

Implementation: `#[file("/AGENT.md")]` returning `include_str!("AGENT.md")`.
Literal routes win precedence over `/{owner}` so the file does not
conflict with the dynamic root projection. The static-child derivation
in `MountRegistry::static_entries_for_parent` injects `AGENT.md` into
the root listing automatically, so `ls <mount>` surfaces it without a
provider call.

Tradeoff: per-provider hand-written file. Could be auto-generated from
the mount manifest later; not worth doing in the PoC. Per-directory
`INDEX.md` is deliberately out of scope — too much per-handler
boilerplate for the value it adds at this point.

### 2. `summary.md` projected sibling on numbered resources

Every issue and PR projects an extra sibling file `summary.md` alongside
`title`, `body`, `state`, `user`. The summary is a ~300-token
markdown bundle:

```markdown
# Issue #123: Title goes here

state: open      author: raulk

<first ~1000 chars of body, line-wrapped>
```

Two emit sites: the lookup-time path (`issue_projection`) emits
`summary.md` as `file_with_content`; the list-time path
(`numbered::preload_common_fields`) emits it via `projection.preload`.
Both paths already hit the GitHub REST payload that contains the source
fields, so the cost is zero extra round trips and ~300 bytes of cache
per issue.

Why a markdown bundle and not just the existing four sibling files: the
agent shouldn't have to do four `cat`s and stitch them together. One
`cat summary.md` returns a self-contained answer. The four primitive
files stay for agents that want one specific field.

Tradeoff: small duplication of bytes vs. round trips. Round trips are
the dominant cost; bytes are free.

### 3. `.events` tailable file at the mount root

The provider keeps a small in-memory ring buffer of recent
`on_event` invocations. Each entry records timestamp, event kind,
and a brief summary (e.g. timer-tick → list of repos polled +
invalidations applied). The buffer is exposed at `<mount>/.events`
as NDJSON.

On every `on_event` terminal the provider also adds `/.events` to
`event-outcome.invalidate_paths`, so the host evicts the cached file
content. A polling agent doing `cat .events` (or `tail .events` in a
loop) sees fresh content on the next read; the kernel page cache
serves stale reads in between, which is fine for the polling cadence.

Tradeoff: this is poll-tail, not stream-tail. True `tail -f` would need
FUSE inotify wiring on the host, which is out of PoC scope. Polling
every few seconds is sufficient for an agent loop and trivially
implementable with the primitives we already have.

### 4. Query directories `_q/issues/{query}` and `_q/prs/{query}`

New dir handlers under a shared `_q` namespace synthesize listings of
issues or PRs matching an agent-supplied query. The query segment is
appended verbatim to GitHub's Search API qualifier:

```
ls /github/raulk/omnifs/_q/issues/state:open+author:raulk+label:bug
```

`{query}` is a single path segment; spaces become `+` (GitHub Search
syntax). Results are projected just like `_issues/_open/`: numbered
directories with the usual `summary.md`, `title`, `body`, `state`,
`user` siblings preloaded. The provider injects `is:issue` or `is:pr`
into the search so the route name keeps its meaning even when the
user's query is permissive.

The query routes live under `_q/...` rather than nested inside
`_issues/...`/`_prs/...` because the path validator's pairwise
overlap check otherwise flags
`/{owner}/{repo}/_issues/q/{query}/{number}` against
`/{owner}/{repo}/_issues/{filter}/{number}/comments` as ambiguous —
both share the same precedence key (literal count, segment length).
Hoisting queries to a sibling top-level keeps the position-3 literal
distinct from `_issues`/`_prs` and resolves the conflict cleanly.

A no-op parent at `/{owner}/{repo}/_q` plus stub parents at
`/{owner}/{repo}/_q/issues` and `/{owner}/{repo}/_q/prs` exist so the
intermediate paths list and look up cleanly. Queries themselves are
not enumerable — `ls _q/issues/` returns an empty listing.

Tradeoff: namespace explosion is bounded by the agent's behavior (it
only types paths it actually wants). For PoC the listing is capped at
one Search page (100 results) and marked non-exhaustive when there
are more matches, so the host doesn't deny later lookups via cached
negatives. REST pagination on top of search is reserved for a future
iteration.

## What's deferred

- Per-directory `INDEX.md` synthesis from manifest.
- A WIT-level `search` capability (typed query, ranked hits, snippets)
  rather than the path-segment hack.
- Cross-mount symlinks for "this PR mentions issue 42" backlinks.
- `.events` as a true streamable file with FUSE inotify wakeups.
- Writes via FUSE `write` syscalls plumbed through `reconcile`.

## Validation

The PoC ships in the github provider. Acceptance criteria:

1. `cat <mount>/AGENT.md` returns the guide on first boot, no API call.
2. `ls <mount>/raulk/omnifs/_issues/_open/123/` lists `summary.md`.
   `cat summary.md` returns the bundled markdown without an extra round
   trip after the parent listing has been fetched.
3. After a timer tick that polls active repos, `cat <mount>/.events`
   returns at least one NDJSON line with the tick metadata.
4. `ls <mount>/raulk/omnifs/_issues/q/state:open+author:raulk` returns
   matching issue directories, each with the same projected siblings.
