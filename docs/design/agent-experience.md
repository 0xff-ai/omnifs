# Agent experience: features and conventions for LLM-driven use of omnifs

Status: proposed
Scope: `wit/provider.wit`, `crates/host` (caching, dispatch, mutation, watching, identity registry), `crates/cli`, `crates/omnifs-sdk` (macros, conventions), provider authoring conventions

## Context

omnifs exposes external services as filesystem trees. The shell-tool affordances that make this useful to humans (`ls`, `find`, `grep`, editor open) compose directly with the tools LLM agents already use (Read, Edit, Glob, Grep, Bash). As agents become a primary user, omnifs becomes load-bearing for agent ergonomics: predictable paths the agent can *compose* without listing, mutations expressible as file edits, surfaces designed for cheap orientation, and safety rails that don't depend on the agent being well-behaved.

This document consolidates a design exploration of features that make omnifs maximally useful to LLMs while preserving (and improving) usefulness for humans. None of it is implemented; it is the agreed shape we want to build toward.

### The mental model

Three surfaces, distinct and complementary:

1. **Browse via FUSE paths.** Standard `Read`, `Edit`, `Glob`, `Grep`, `Bash` (`ls`, `cat`, `find`, `grep`) work. The optimization target is *path composition without prior listing*: agents should be able to construct `Read /omnifs/github/raulk/omnifs/issues/42/body.md` from prior knowledge, not from `ls` round trips.
2. **Mutate via git commits on the working tree.** Changesets *are* git commits. `omnifs push` translates the working-tree diff into upstream API calls. Agents inherit the entire git ecosystem: `diff`, `blame`, `bisect`, `worktree`, hooks, attributes, conflict resolution.
3. **Type and query via the `omnifs` CLI.** A typed-ish surface for what paths handle awkwardly: orientation, search-with-filters, multi-arg planning, cost queries, push management. The CLI is the *only* typed surface; no MCP layer in v1.

Everything below is in service of these three surfaces.

## Decisions

### D1. Record skeleton (the standard shape)

A "record" is any external entity that has fields, state, sub-collections, and events: a GitHub issue, a calendar event, an email thread, a DNS zone. Records share a single shape so agents that learn one provider know all of them:

```
<record-id>/
  title                    # one-line text, edit-in-place
  body.md                  # primary content, edit-in-place
  state                    # canonical state token, edit-in-place
  metadata.json            # read-only canonical record
  labels/                  # set: touch <name> to add, rm <name> to remove
  assignees/               # same shape as labels/
  comments/                # existing comments, each <id>/ a sub-record
  comments/_outbox.jsonl   # append-only events drained on push
  _events.jsonl            # tail-able event stream (host-managed)
  _wait/any-event          # blocking read; wakes on next event
  _backlinks/              # symlinks to references-in
  _caps                    # what's writable here
  _schema.json             # machine-readable route descriptor
  _describe.md             # prose orientation
  _notes/                  # agent-authored markdown
  _diff                    # working-tree diff for this subtree
  _pending                 # symlink to in-flight op (only when present)
  _error.json              # only present on failure
```

Conventions that lock this in:
- Listings sorted lexicographically; `_`-prefixed entries surface first.
- JSON is sorted-key, two-space indent, trailing newline.
- Timestamps RFC 3339 UTC.
- Symlinks relative within a provider, absolute when crossing.
- Empty-known files exist with empty content (preferred over absence; saves a lookup round trip and disambiguates "doesn't exist" from "is empty").

### D2. Mutation primitives

Four field shapes, declared per-field in `_schema`. Providers compose records out of them.

| Kind     | Surface                          | Write semantics                    | Push translates to             |
|----------|----------------------------------|------------------------------------|--------------------------------|
| `edit`   | Plain file (`title`, `body.md`)  | Overwrite                          | PATCH field                    |
| `state`  | Single-token file (`state`)      | Overwrite, validated against enum  | PATCH state, with transitions  |
| `set`    | Directory of empty files         | `touch`/`rm` to add/remove members | PATCH set (union/diff)         |
| `outbox` | Append-only `_outbox.jsonl`      | Append a JSON line per intent      | Drain: one upstream call/line  |

`edit`/`state` mutate state; `set` mutates membership; `outbox` emits events. Three primitives cover create-comment, edit-body, close-issue, add-label, assign-user — the long tail of upstream API verbs collapses into them.

Per-field merge semantics declared in `_schema` for git's three-way merge driver:
- `set` fields: union merge driver (concurrent adds union; concurrent removes apply both).
- `state` fields: last-writer-wins or refuse-and-conflict (declared per field).
- `edit` fields: standard text three-way; conflict markers if same hunk.
- `outbox` fields: append-only, no conflict possible.

This composes with `.gitattributes` so concurrent agent sessions on the same mount resolve cleanly within disjoint subtrees and surface real conflicts only on same-field same-hunk edits.

### D3. Discovery surfaces — `_schema.json` + `_describe.md`

Two files, two audiences.

**`_schema.json`** at every subtree, machine-readable. Per-route entry:

```json
{
  "schema-version": "1",
  "provider-version": "github@0.4.2",
  "pattern": "/repos/{owner}/{name}/issues/{id}/",
  "kind": "record",
  "cost": "medium",
  "scopes-required": ["issues:read"],
  "freshness": {"max-age": "5m", "refresh-on-read-if-stale": true},
  "mode": "sync",
  "fields": [
    {"name": "title", "kind": "edit", "format": "text",
     "required-at-create": true, "scope": "issues:write"},
    {"name": "state", "kind": "state",
     "values": ["open", "closed"],
     "transitions": {"open":["closed"], "closed":["open"]},
     "scope": "issues:write"},
    {"name": "labels/", "kind": "set", "merge": "union",
     "scope": "issues:write"},
    {"name": "comments/_outbox.jsonl", "kind": "outbox",
     "drain-on-push": true, "scope": "issues:write"}
  ],
  "examples": [
    {"goal": "close an issue with a parting comment",
     "steps": [
       "echo closed > state",
       "echo '{\"body\":\"shipped\"}' >> comments/_outbox.jsonl",
       "git commit && omnifs push"]}
  ]
}
```

**`_describe.md`** at every subtree, hand-authored prose: 2–5 paragraphs covering "what this is", "primary verbs", "common gotchas", "see-also paths". No machine consumption; pure orientation.

Rule of thumb: anything parseable goes in `_schema`. `_describe` is for nuance prose can convey better than structure.

`_schema` versioning is non-optional; recipes (D13) embed the `schema-version` they were recorded against and warn loudly on replay if the schema has drifted.

### D4. `_notes/` — agent-authored markdown per subtree

A writable directory the host owns and persists *outside* the upstream-mirroring tree. Markdown files, free-named, never pushed upstream.

- Each note carries `.meta/{author, session-id, written-at}`.
- Convention: `_notes/agent-runbook.md` per subtree captures cross-session learnings about operating that subtree. The next agent (or me, in a future session) starts from that knowledge instead of relearning from scratch.
- `omnifs explain <path>` reads `_schema` + `_describe` + `_notes/` together. Schema is canonical, describe is authored orientation, notes are tribal knowledge.

Why a separate space: keeps the upstream-mirroring tree clean and pushable, while still allowing a persistent agent-authored layer co-located with the data.

### D5. Mutation protocol — git commits as changesets

The mount is a working tree. Edits are local file changes. A "change" is `git add` + `git commit`. `omnifs push` translates the diff between `.omnifs/last-applied-sha` and `HEAD` into upstream API calls.

#### D5.1 Halt-on-first-failure

The provider attempts ops in the order they appear in the diff; on the first failure it halts. State after a failed push:

- `.omnifs/last-applied-sha` advances only as far as the last successful op.
- `_apply.log` (jsonl, append-only) records every attempted op:
  ```json
  {"ts":"…","op":"patch-state","path":"issues/42/state",
   "request":{…},"status":"ok","response":{…}}
  ```
- Failed op leaves `_apply.errors.json` next to the offending file containing the upstream error and (where applicable) the upstream's current value for stale-ETag conflicts.
- Resume is implicit: edit the offending file, recommit, push. Provider re-derives ops from `last-applied-sha` to current `HEAD`, skips already-applied, executes the rest.

#### D5.2 `_new/` host primitive (creates)

Creating a new record:

1. Agent does `mkdir <collection>/_new/<draft-id>/`, populates required-at-create fields per `_schema`.
2. Agent commits.
3. On push, host scans staged paths, finds `_new/<draft-id>/` subtrees, packages each as a `create-record` op.
4. Provider responds with `{real-id, response-body, sibling-files?}`.
5. Host renames `_new/<draft-id>/` → `<real-id>/` as a follow-up commit, resolves `${_new/<draft-id>/id}` placeholders in other staged files in the same originating commit, writes the response into `metadata.json` and any sibling files.
6. On failure: halt-on-first applies. The `_new/<draft-id>/` survives, gets `_apply.errors.json`, recommit-to-resume.

Host validates against `_schema` before push: a draft missing required-at-create fields fails locally without burning a round trip.

#### D5.3 Conflict resolution

Stale-ETag (upstream changed since fetch) is the common case:

- Provider records the upstream-at-fetch SHA/ETag at fetch time; push includes `If-Match`.
- Stale → halt-on-first with `_apply.errors.json` carrying `{base, local, upstream}` for each affected field.
- For `set` and (some) `state` fields, automatic merge per declared semantics.
- For `edit` fields, provider drops `<file>.LOCAL`, `<file>.UPSTREAM`, `<file>.BASE` next to the original. Agent reconciles, removes the triplet, recommits.

`git pull` in the working tree (refetch) is the recovery path — same as for source-controlled code.

#### D5.4 Reverts

`git revert <sha>` produces a working-tree state the provider re-parses into inverse API calls. Bidirectional diff parsing is real implementation work for providers; document per route in `_schema` (`revertible: true|false`) so agents know what's safe to revert.

> **Open / speculative.** Inverse-op derivation is non-trivial for create-shaped ops (revert of a "post comment" requires the upstream comment id, which the provider knows from `_apply.log` — but revert of a "send email" is irreversible). The `revertible` flag is per-route, but reverting a multi-op commit may have a heterogeneous answer. Likely v1 answer: `revertible` is the conjunction over all ops in the commit; mixed commits require explicit per-op reverts.

### D6. Long-running upstream ops — per-provider `_pending/` tray

Some ops legitimately take time: CI dispatches, batch enqueues, releases. The pending tray decouples agent timing from upstream timing.

```
<provider>/_pending/<op-id>/
  status                    # queued | running | done | failed
  progress                  # 0..100 or stage names
  created-at
  originating-commit
  affects/                  # symlinks → records this op touches
  _events.jsonl             # tail-able op-level events
  _wait/any-event           # block until next progress
```

Affected records carry a back-symlink: `<record>/_pending → <provider>/_pending/<op-id>/`. On completion the host moves entries into `_pending/_done/<op-id>/` for a grace window before deletion. `_all/_pending` aggregates across providers (D8).

#### D6.1 Sync vs pending mode

Each route declares its mode in `_schema`:
- `sync`: blocks until upstream returns; result inline. For sub-second ops.
- `pending`: returns op-id immediately; tracking lives in the tray.

#### D6.2 Auto-escalation on sync timeout

Sync routes declare a sync timeout. If exceeded, host transparently promotes the op to the pending tray and returns to the caller:

```json
{"_promoted-to-pending": {"op-id": "…", "wait-path": "…/_pending/…/_wait/any-event"}}
```

Failed promotion (provider doesn't support resumption) → halt-on-first-failure with `_apply.errors.json`. Documented per provider in `_caps`.

### D7. Liveness — host primitive with provider hooks

The single biggest agent unlock is being able to *wait*. Polling burns context; `sleep` loops are blocked. The host owns liveness primitives; providers source signals.

#### D7.1 `_events.jsonl` and `append_event`

Every subtree has a tail-able `_events.jsonl`. Lines are JSON: `{ts, kind, path, id}`. Host primitive:

```
append_event(provider_id, path, line)
  → appends to the file
  → kicks FUSE_NOTIFY_INVAL_INODE so tail -f wakes up
```

Providers translate upstream events (webhooks, polling deltas, server-sent events) into `append_event` calls via an `on_event(upstream_event)` hook. Providers don't manage files; they emit signals.

#### D7.2 `_wait/any-event` (v1 predicate)

A pseudo-file at every subtree whose `read` blocks until an event is appended to that subtree's `_events.jsonl` after read-start.

- Watermark is the file's append-counter at open time. No race where I miss an event between observing state and entering wait.
- Supports `_wait/any-event?since=<event-id>` for explicit watermarks ("wake when there's news after this id").
- Supports `_wait/any-event?timeout=30s`. On timeout, returns empty (distinguishable from an event by empty body).
- Returns the triggering event line(s) as a small JSON payload.
- Implementation: pseudo-file whose `read` parks on a futex/condvar; `append_event` wakes it.

Predicate vocabulary stays minimal in v1. Agents compose conditions client-side: read state, enter wait, re-read state, loop. Future versions can add a predicate library (`state=`, `count>=`, etc.) once usage patterns settle.

> **Open / speculative.** The any-event watermark protocol needs to handle: (a) the file being truncated/rotated under `_events.jsonl` retention, (b) reads that span multiple events arriving as a batch, (c) ordering of events under provider concurrency, (d) what happens if a provider crashes mid-event. None of this is hard, but the exact wire shape (event-id format, monotonicity guarantees, retention behavior) needs concrete spec before implementation.

### D8. Cross-provider — `_id/` and `_all/`

#### D8.1 `_id/` — identity registry

Top-level virtual root for cross-provider entity identity:

- Providers declare entity types they own in their manifest (`github` owns `User`, `Repo`, `PullRequest`; `calendar` owns `Calendar`, `Event`).
- Canonical path: `/_id/<type>/<key>` is a symlink the provider registers on first sight.
- References within records (e.g. an issue's `author`) are symlinks into `/_id/`. The link survives provider re-mount, repo rename, schema changes.
- Conflicts (multiple providers claim the same `<type>/<key>`): expose `/_id/<type>/<key>/_providers/` listing claimants; canonical is the one with priority configured at mount time.
- Unresolved references leave `<thing>.<ref>._error.json` rather than dangling symlinks.

> **Open / speculative.** Identity registration races: provider A is mounted, references provider B's entity before B is mounted — what does the symlink resolve to? Likely the host buffers references, resolves on later registration, and surfaces `_error.json` only after a configurable grace window. Also: what is the canonical *key* for an entity that two providers identify differently (GitHub email vs Calendar email vs Slack handle)? v1 answer: keys are provider-declared per type; cross-provider identity requires a separate alias layer (out of v1 scope).

#### D8.2 `_all/` — aggregator mount

Top-level mount that aggregates cross-cutting views:

```
/omnifs/_all/
  _recent.jsonl   # time-merged event stream from all mounts
  _id/            # canonical identity registry
  _search         # federated search; fans out to each provider's _search
  _caps           # mount manifest: what's mounted, granted scopes
  _quota          # every provider's quota in one read
  _apply.log      # every push, time-merged audit trail
  _pending/       # symlinks to all per-provider _pending/ trays
  _mounts         # listing of every mount
```

`tail -f /omnifs/_all/_recent.jsonl` is the single ambient-awareness target across the whole filesystem. `_all/_apply.log` is the cross-mount audit trail.

### D9. Cost and safety

#### D9.1 Cost class per route

`_schema` declares each route's cost class:
- `cheap`: cached or local.
- `medium`: one upstream call.
- `expensive`: paginated, multi-call, or rate-limit-impacting.

Each freshly-fetched payload has a sibling `.meta/cost.json` recording actual cost (calls made, tokens consumed, duration). Lets agents calibrate static estimates against reality.

#### D9.2 `_quota` — live rate-limit visibility

Read-only virtual file at provider root:

```json
{
  "limits": {"requests-per-hour": 5000, "tokens-per-day": 100000},
  "used":   {"requests-this-hour": 234, "tokens-today": 18472},
  "remaining": {"requests-this-hour": 4766},
  "resets-at": "2026-05-08T16:00:00Z"
}
```

Updates in near-real-time as ops happen. `_all/_quota` aggregates across providers.

#### D9.3 `_budget` — agent-imposed caps

Read-write virtual file per session, mount, or user:

```json
{"max-requests": 500,
 "max-write-ops": 20,
 "on-exhaustion": "halt",
 "on-write": "live"}
```

Most-restrictive across scopes wins. A write that would exceed budget halts with `_error.json` rather than partially executing.

#### D9.4 Dry-run by default for new mounts

A freshly mounted provider auto-creates `_budget` with `{on-write: "dry-run"}`. Pushes execute the diff parse and write `_apply.log` (with `[dry-run]` prefixed lines), but skip upstream calls.

Flipping out of dry-run requires `omnifs go-live <mount>`, which:
1. Shows the planned-but-not-applied ops from recent dry runs.
2. Requires explicit confirmation.
3. Replaces the budget with one allowing live writes.

An agent operating without explicit live-write authority is structurally incapable of touching upstream — strong default, costs nothing in browse-heavy workflows.

#### D9.5 Pre-fetch cost gate for `expensive` routes

Reading an `expensive` path the first time returns:

```json
{"_estimate": {"calls": 50, "tokens": 5000, "duration-est-ms": 8000,
               "confirm-token": "ct_abc123", "expires-at": "…"}}
```

To get real content: write the token to sibling `_confirm` (single-use, time-bounded), then re-read. Budget debits at confirm time.

`omnifs cost <path>` produces the same estimate without minting a token.

Per-mount `confirm-policy: {auto-confirm-under: 100-calls}` removes the gate for known-cheap-enough scans, keeping friction in the right place.

> **Open / speculative.** Estimation accuracy is the load-bearing assumption here. Some providers can give tight bounds (paginated APIs with known total-count headers); others can only estimate from prior fetches. The `_estimate` shape should probably include `confidence: "exact" | "bounded" | "heuristic"` so agents know whether to trust a low estimate. Also: should the host *enforce* the estimate (refund unused budget on under-runs, halt on over-runs)? v1 answer: debit the estimate at confirm; reconcile actuals to `_quota.used` after the call returns; over-runs surface as a warning event but don't halt.

#### D9.6 Backpressure under quota pressure

When `_quota.remaining` drops below threshold, host throttles fetches transparently and surfaces a warning event in `_recent.jsonl`. Pushes block (rather than partially failing) with `_error.json` until headroom returns.

### D10. Identity and provenance — always-the-human

Upstream writes always execute under the human's account. Agent provenance lives locally:

- `metadata.json` of created records carries `created-by-agent: <session-id>`.
- `_apply.log` records every op with session-id.
- The git history of the working tree IS the agent attribution layer; `git blame` on any field-file shows which session set what.

Optional opt-in: provider injects a small machine-readable trailer in agent-authored prose, e.g. `<!-- omnifs: session=abc123 -->` for HTML-tolerant fields, `\n[applied via omnifs]` for plain text. Off by default; configurable per mount.

Multi-user-multi-agent on one mount: standard `flock` on a record root for explicit claim. `_who/` listing active sessions. Convention: agents work in disjoint subtrees by default; `git`'s merge resolves the rest.

> **Open / speculative.** "Always the human" is the v1 default but not necessarily the right long-term answer. Some providers (Slack, GitHub Apps, Jira service accounts) cleanly support bot identities with first-class API tokens; impersonating-the-human there discards real provenance the upstream itself wants. A future iteration may want a `identity-mode: human | agent | session-bot` per mount, with provider-declared support for each. Worth revisiting once we have a concrete provider whose upstream models bots well.

### D11. CLI surface

The CLI is the typed-ish surface alongside FUSE paths. No MCP layer in v1.

#### D11.1 Subcommands

```
omnifs orient [<mount>] [--all]      # onboarding digest
omnifs explain <path>                 # _schema + _describe + _notes/
omnifs cost <path>                    # estimate without debit
omnifs grep <query> [<path>]          # federated search
omnifs find <path> -<predicate>       # structured query via _schema facets
omnifs outline <path>                 # paths-only tree, depth-limited
omnifs cat <path>                     # streaming read (handles tail-able)
omnifs push [<mount>] [--dry-run]     # apply pending changes
omnifs pending [<mount>]              # list in-flight ops
omnifs wait <path> [--timeout=]       # block on _wait/any-event
omnifs go-live <mount>                # flip out of dry-run
omnifs budget <mount> [--set …]       # read or write _budget
omnifs why <path>                     # provenance: provider, fetched-at, cost
omnifs trace <path>                   # diagnostic: cache hits, callouts, durations
omnifs refresh <path>                 # cache bust
omnifs pin <path> <name>              # snapshot
omnifs diff <path> [<pin>]            # diff vs pin or working tree
omnifs notes <path> [--add|--list]    # read/write _notes/
omnifs replay <sha>                   # re-apply past commit
```

#### D11.2 Agent ergonomics

- `--json` everywhere; the human-pretty form is opt-in via `--pretty`. Default to JSON when stdout is non-TTY.
- Stable correlation IDs per command (`x-omnifs-correlation-id` in headers, mirrored in JSON output and `_apply.log`).
- Stable exit codes:
  - `0` success
  - `1` usage error
  - `2` budget exhausted
  - `3` upstream error
  - `4` conflict (stale ETag, three-way merge needed)
  - `5` unauthorized scope
  - `6` provider down or `_health != up`
- Pipe-friendly: `omnifs find … | omnifs explain --stdin` chains cleanly.
- `--session=<name>` for parallel work without coordinating budgets/notes.

#### D11.3 `omnifs orient <mount>` shape

One round trip; agent has enough to start work:

```json
{
  "mount": "github",
  "describe": "…prose from _describe.md…",
  "caps": {"writable": true, "scopes": ["repo:read","issues:write"]},
  "primary-routes": [
    {"pattern": "/repos/{owner}/{name}/issues/{id}/",
     "cost": "medium", "writable": true,
     "see-recipe": "_examples/close-issue"}
  ],
  "common-recipes": [
    {"name":"close-issue","steps":[…]},
    {"name":"add-label","steps":[…]}
  ],
  "budget": {"on-write": "dry-run"},
  "quota": {"req-remaining": 4823, "resets-at":"…"},
  "health": "up",
  "recent": ["2026-05-08T14:22:01Z agent-push session=X ops=2"]
}
```

`omnifs orient --all` returns a `{"mounts":{…}}` map across every mount.

### D12. Auto-recipes — `_examples/recorded/`

After every successful push, host:

1. Extracts the diff that was applied.
2. Parses the ops via the same parser used for push.
3. Sanitizes values: replaces concrete ids/owners with `<…>` placeholders, truncates long bodies, strips PII per provider-declared rules.
4. Names the recipe from the parsed plan (`close-issue`, `add-label-to-pr`); collisions get suffixes.
5. Writes to `<provider>/_examples/recorded/<verb>-<object>/` with sidecar metadata: `commit`, `recorded-at`, `redaction-rules`, `count` (incremented on dedupe).

Agents reading `_examples/` see hand-authored and recorded examples interleaved, marked by `source: "recorded" | "authored"`. Bootstraps the example corpus without anyone hand-writing it.

Opt-out: mount config `record-recipes: false`; per-route opt-out via `_schema` flag.

> **Open / speculative.** Sanitization rules are the hard part. Generic redaction (mask anything that looks like an id, email, token) is too aggressive and will eat real content; provider-declared rules (`#[redact(field = "owner")]`) are precise but require author work. Likely answer: ship a generic baseline (replace path captures with placeholders, truncate body fields above N bytes), let providers override per-field via attribute. Also unresolved: how do recorded recipes age — when `schema-version` advances, do old recipes auto-rewrite, get marked stale, or disappear?

### D13. Author-side macros

Provider authors declare; surface is derived. Three macros, all generating into the same dispatch and listing layer:

```rust
#[record]
struct Issue {
    #[field(edit)]                title: String,
    #[field(edit, format = "md")] body: String,
    #[field(state, values = ["open","closed"],
            transitions = {"open":["closed"], "closed":["open"]})]
    state: String,
    #[field(set, merge = "union")] labels: Vec<String>,
    #[field(set)]                  assignees: Vec<UserId>,
    #[field(outbox)]               comments: Outbox<NewComment>,
    #[event]                       events: EventStream,
}

#[index(by = labels, by = state, by = assignee)]
struct IssueCollection { /* … */ }

#[event(emits = [IssueOpened, IssueClosed, CommentAdded])]
impl on_upstream_event for Issue { /* … */ }
```

Generated:
- All field-shape files (`title`, `body.md`, `state`, `labels/`, `assignees/`, `comments/_outbox.jsonl`).
- `_schema.json` entry with declared semantics.
- `_caps`, `_describe.md` skeleton (author fills in prose).
- `_events.jsonl` and `_wait/any-event` wiring.
- `_by/labels/<x>/`, `_by/state/<x>/`, `_by/assignee/<x>/` symlink trees.
- Three-way merge driver registration in `.gitattributes`.
- `_id/` registration for entity-type owners.

Provider authors describe data; the agent surface is derived end-to-end.

> **Open / speculative.** The `#[record]` macro is record-centric; it doesn't fit cleanly for non-record-shaped data (DNS zones, time series, blob stores, log streams). Those probably want their own macros (`#[zone]`, `#[stream]`, `#[blob]`) generating different conventions. v1 should at least specify what falls through to manual handler registration vs what's covered by macros, so authors don't try to force non-records into `#[record]`.

### D14. Git-native power moves

Because changesets are commits, agents inherit the entire git toolbox. `_describe.md` for every mount should call these out so agents know to reach for them:

- `git blame <field-file>` — who/when last set this field.
- `git log -p comments/_outbox.jsonl` — full append history of agent-posted comments.
- `git bisect` over agent-applied commits — find when something broke.
- `git worktree add <path> <branch>` — parallel "draft realities" of the same mount, one per task, no coordination cost.
- `.gitattributes` per record type with custom merge drivers (D2): `labels/* merge=union`, `body.md merge=text-3way`, `state merge=last-writer-wins`.
- `.gitignore` for files that surface in the tree but shouldn't commit.
- Tags as bookmarks: `git tag pre-cleanup` before a risky multi-record edit; `git reset --hard pre-cleanup` to abandon.
- Standard pre-commit / pre-push hooks complement `.omnifs/hooks/`.

### D15. Snapshots and time travel

- `omnifs pin <path> <name>` captures a subtree state as a frozen read-only mount under `_pins/<name>/`. Survives mount restarts; `_pins/_index` lists them.
- `omnifs diff <path> [<pin>]` shows what's changed since a pin (or working tree).
- `<subtree>/_as-of/<sha>/` resolves to the working-tree contents at that commit. Useful for "what did this look like before my last push" without `git checkout` shenanigans.

### D16. Cache-bypass and freshness

- Per-route freshness policy in `_schema` (`max-age`, `refresh-on-read-if-stale`). Host auto-refreshes during a normal `Read` if the cached entry exceeds policy.
- `_fresh/<path>` mirror always re-fetches upstream, ignoring host cache. Cheap escape hatch when an agent knows upstream just changed.
- `omnifs refresh <path>` busts cache for a subtree on demand.
- `.meta/fetched-at`, `.meta/etag`, `.meta/source-url` provenance sidecar on every fetched payload. Agents check `fetched-at` to gauge staleness without round-tripping.

### D17. Diagnostic surfaces

- `_health` per provider: `up | degraded | down` plus `last-error.json` and provider version.
- `_metrics` per route: p50/p99 latency, recent error rate, refresh-hit rate.
- `omnifs trace <path>` shows the full resolution path: provider, route, cache layers hit/missed, callouts issued, total latency.
- `omnifs why <path>` shows provenance: which provider, which route, when fetched, cost, freshness.

### D18. Cross-mount transactions (v1: none)

- A single commit can span mounts; push is per-mount, no cross-mount atomicity.
- `_caps` declares this honestly so agents don't assume saga semantics.
- Future: a coordinator that records compensating actions per op so a cross-mount failure can roll back. Out of scope until two specific providers actually need it.

## Lifecycle of an agent operation (illustration)

To anchor the abstractions, here is a complete end-to-end agent workflow against a freshly mounted GitHub provider:

1. Agent runs `omnifs orient github`. One read; receives `describe`, `caps`, primary routes, common recipes, current budget (`{on-write: dry-run}`), quota.
2. Agent decides budget: `omnifs budget github --set max-write-ops=10 max-requests=200`.
3. Agent doesn't go live yet — wants to test the workflow first.
4. Agent reads `Read /omnifs/github/raulk/omnifs/issues/_examples/close-issue/_describe.md`. Gets the canonical recipe.
5. Agent identifies issue 42, edits `state` from `open` to `closed`, appends `{"body":"shipped"}` to `comments/_outbox.jsonl`.
6. `git commit -m "close 42"`.
7. `omnifs push --dry-run`. Output: `[dry-run] would PATCH .../issues/42 {"state":"closed"}; would POST .../issues/42/comments {…}`.
8. Output looks correct. Agent runs `omnifs go-live github`, confirms.
9. `omnifs push`. Halt-on-first hits 200 OK on both ops; `_apply.log` records them; `last-applied-sha` advances.
10. Host auto-records the recipe at `github/_examples/recorded/close-issue/`.
11. Event arrives via `on_event` hook → `append_event` writes a line to `issues/42/_events.jsonl` and `_all/_recent.jsonl`.
12. Agent (or a watching agent) sees the event, validates state, writes a note: `omnifs notes /omnifs/github/.../issues/42 --add "closed in session X, see commit abc123"`.

Every step composes existing tools (Read, Write, Edit, Bash for `git`, `omnifs` for the typed surface). No agent-specific magic.

## Open questions

The decisions above are agreed in shape. The questions below are *not*, and need design work before implementation. They are signposts for future brainstorming.

### Substantive (block implementation of the related decision)

- **Predicate library beyond `_wait/any-event`.** v1 ships only the bare any-event wait. Once usage patterns emerge, design questions: which predicates are host-canonical vs provider-supplied, what is the predicate composition grammar, do timeouts compose with predicates, is there a `_wait-any` across multiple paths in one read.
- **Conflict resolution UX in detail.** D5.3 sketches the LOCAL/UPSTREAM/BASE pattern, but the actual three-way diff format for non-text fields (set membership, state transitions, structured JSON) is not specified. Likely needs per-field-kind merge protocols.
- **Inverse-op derivation for revert.** D5.4 names the problem; the actual provider authoring story is open. Probably needs a `#[revert]` macro that pairs forward/inverse in declaration so the provider doesn't write inverse parsers by hand.
- **`_id/` keying and reconciliation.** D8.1 punts cross-provider key reconciliation to a future "alias layer". That layer needs design before any cross-provider linking ambition is realistic.
- **Cost estimation accuracy and confidence levels.** D9.5 calls this load-bearing; the actual confidence taxonomy (`exact | bounded | heuristic`) and how `_budget` enforcement interacts with it needs spec.
- **Auto-recipe sanitization rules.** D12 is a sketch; the redaction baseline + per-field overrides need concrete rules and a way to test "would this leak anything" before recipes ship to recorded/.
- **Schema migration protocol.** When `schema-version` advances, what happens to recorded recipes, embedded examples, and agent scripts that referenced old paths? Probably needs a migration grammar (rename/move/remove) the host can apply automatically.
- **Concurrent push semantics.** Two agents push to the same mount simultaneously: who wins, what locks where. Likely a per-mount push lock with fairness, but the queueing semantics (FIFO? priority? cancel-in-flight?) need design.
- **Streaming reads + FUSE size reporting.** Tail-able files don't have a knowable size. Interaction with `ls -l`, `du`, `head -c $size` needs the same kind of treatment `projected-file-sizes.md` gave to projected files.
- **Non-record entity macros.** `#[record]` doesn't fit DNS zones, log streams, blob stores. Need parallel macros (`#[zone]`, `#[stream]`, `#[blob]`) or a more general declaration grammar.

### Speculative (worth exploring; not blocking)

- **Identity model evolution.** Always-the-human is v1. Bot/agent/session-bot identity per mount may be the better long-term answer for upstreams that natively support it (D10 callout).
- **Cross-mount saga / compensating actions.** Out of v1. Worth revisiting once a real workflow needs atomicity across two providers.
- **Replay-against-fixtures for deterministic agent tests.** Substrate exists (auto-recipes + cached responses); the test-harness UX (env var? CLI flag? mount-time mode?) is unspecified.
- **Permission scope narrowing per session.** A session asking for *less* than the mount grants, for sandboxing sub-tasks. Useful but not blocking; layer it onto `_caps` later.
- **Read prefetch hints from `_schema`.** Provider declares co-fetch patterns; host prefetches. Defer until hot patterns are observed.
- **MCP tool surface.** Out of scope for v1; revisit if there's demand. Cheapest path is auto-derivation from `_schema` writable routes.
- **Trust / sandboxing model for provider WASM.** WASM components are sandboxed by default, but secret access (API tokens) and fine-grained capability passing aren't pinned down here. Design out of this doc's scope but interacts with D9 (scopes) and `permissions.json`.
- **`humans/` convention for assignees/contacts.** Lightweight idea; defer until a real workflow needs it.
- **`_focus` / session path templates.** Quality-of-life for agents in long workflows. Defer.
- **Live cost meter over `_wait`.** Agents observing `_budget.used` cross thresholds. Useful but reachable post-v1.
- **Path stability under upstream renames.** `_renamed-from/` redirect symlinks with TTL. Concrete need depends on provider behavior.

### Out of scope

- True cross-mount atomicity (sagas).
- An MCP surface in v1.
- Multi-tenant omnifs (multiple humans, hierarchical agent identities) beyond the single-user-multi-agent case.

## Priorities and ROI

The decisions above describe a substantial design surface. Below is a recommendation on which parts to build first based on agent-impact relative to implementation cost.

### Tiering

**Tier 1 — Foundation (build first; nothing else lands cleanly without these).**
Without these, agents either can't operate omnifs at all or operate it unsafely.

| Feature | Section | Agent impact | Cost |
|---|---|---|---|
| Record skeleton + four mutation primitives | D1, D2 | Defines the shape every other layer plugs into. | Medium (SDK + dispatch). |
| `#[record]` macro | D13 | Without it, every provider authors the skeleton by hand and conventions drift. | Medium. |
| `_schema.json` + `_describe.md` | D3 | The primary discovery surface; agents need to compose paths without listing. | Low (mostly emit-from-macro). |
| `_apply.log` + halt-on-first push | D5.1 | Mutation cannot ship without it; agents need a reliable resume story. | Medium. |
| Dry-run-by-default + `_budget` | D9.3, D9.4 | Safety rail for unfamiliar mounts; the structural protection for "the agent ran amok". | Low (a single budget file gate). |
| `omnifs explain` CLI | D11.1 | Without one read returning schema + describe + notes, every operation costs N round trips of orientation. | Low. |
| Cost class annotations in `_schema` | D9.1 | Pure metadata; lets agents triage before paying. | Trivial. |

**Tier 2 — Transformative (ship as soon as Tier 1 is stable).**
These multiply agent capability dramatically; they're the difference between "I can read and edit" and "I can operate autonomously".

| Feature | Section | Agent impact | Cost |
|---|---|---|---|
| `_events.jsonl` + `_wait/any-event` | D7 | Single biggest unlock for autonomous workflows. Without it, agents poll and burn context. | Medium-high (FUSE notifier + pseudo-file blocking read). |
| `_new/` host primitive | D5.2 | Unlocks creates. Without it, "create a new issue" is per-provider bespoke. | Medium (commit rewriting + placeholder resolution). |
| `omnifs orient` | D11.3 | Cuts first-mount cost from N round trips to 1; sets agents up for everything else. | Low (composes existing surfaces). |
| `_quota` virtual file | D9.2 | Lets agents calibrate ambition against real headroom. | Low (provider already tracks this). |
| `_notes/` per subtree | D4 | Cross-session memory; agents stop relearning what they learned. | Low (a writable persistent dir). |

**Tier 3 — Force multipliers (ship once Tier 2 is solid).**
High agent impact, but builds on more mature infrastructure.

| Feature | Section | Agent impact | Cost |
|---|---|---|---|
| `_id/` registry | D8.1 | Cross-provider navigation via symlink. | High (entity-type declaration, registration races). |
| `_all/` aggregator mount | D8.2 | Single ambient-awareness target; cross-cutting search. | Medium (mostly view layers over per-provider data). |
| `_pending/` tray + sync-vs-pending | D6 | Long-running ops without polling. | Medium-high (state machine + escalation). |
| Auto-recipes | D12 | Bootstraps the example corpus; agents learn from successful ops. | Medium (sanitization rules are subtle). |
| Pre-fetch cost gate | D9.5 | Hard cap on accidental expensive reads. | Medium (token state, expiry, debit). |
| `#[index]`, `#[event]` macros | D13 | Reduces per-provider boilerplate; standardizes graph navigation. | Medium. |
| Conflict resolution (`.gitattributes` merge drivers) | D2, D5.3 | Multi-agent and multi-session concurrency without explicit locking. | Medium (per-field merge drivers). |

**Tier 4 — Operational maturity (defer until production usage demands it).**
Real value but the absence doesn't block agents from doing useful work.

| Feature | Section | Agent impact | Cost |
|---|---|---|---|
| Snapshots / `_pins/` / `_as-of/` | D15 | Stable views during long reasoning. | Medium. |
| `omnifs trace` / `omnifs why` | D17 | Debugging "why is this slow / stale". | Low-medium. |
| `_health` / `_metrics` | D17 | Production observability. | Low. |
| `_fresh/` cache-bypass mirror | D16 | Escape hatch for known staleness. | Low. |
| `_diff` virtual file | D1 | Convenience over `git diff -- <path>`. | Trivial. |
| `_session/` scratchpad | (mentioned in brainstorm, not formalized) | Per-session private space. | Low. |
| `_latest/` symlinks | (mentioned in brainstorm, not formalized) | "Most recent" shortcut. | Low. |
| Reverts with inverse-op parsing | D5.4 | `git revert` actually undoes upstream. | High (per-provider implementation). |

### Suggested build order

A conservative path, each step shipping a real agent affordance and not dependent on later steps:

1. Tier 1 in full. This is the minimum viable surface.
2. `_events.jsonl` + `_wait/any-event` from Tier 2 (biggest single unlock).
3. `_new/` + `omnifs orient` + `_notes/` from Tier 2.
4. `_id/` + `_all/` from Tier 3 (unlock cross-provider stories).
5. `_quota` + cost gate from Tier 3 (now that Tier 2 ambition exists, harden the safety stack).
6. `_pending/` tray, auto-recipes, remaining Tier 3 macros.
7. Tier 4 piecemeal as production usage surfaces concrete needs.

### Where ROI is highest (TL;DR)

If only three things shipped beyond Tier 1, they should be:

1. **`_wait/any-event`** — turns polling agents into event-driven agents. Single largest behavior change.
2. **`omnifs orient`** — the difference between "agent flounders for the first 20 reads on a new mount" and "agent is productive on read 1".
3. **`_id/` + `_all/_recent.jsonl`** — turns omnifs from "a filesystem of services" into "a graph of services with ambient awareness", which is the actual mental model agents want.

If only one thing shipped: `omnifs orient` plus a stable `_schema` + `_describe` shape. Without orientation, every other feature is hidden behind the agent's exploration cost.
