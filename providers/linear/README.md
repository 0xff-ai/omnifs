# omnifs-provider-linear

A Linear provider for omnifs. Exposes Linear teams and issues as a virtual filesystem.

## Path tree

```
/linear/teams/                              # all workspace teams by key
/linear/teams/{KEY}/                        # one team (e.g. ENG, OPS)
/linear/teams/{KEY}/issues/                 # filter selector
/linear/teams/{KEY}/issues/_all/            # all issues for the team
/linear/teams/{KEY}/issues/_open/           # open issues (triage, backlog, unstarted, started)
/linear/teams/{KEY}/issues/{filter}/{KEY-N}/title           # short title
/linear/teams/{KEY}/issues/{filter}/{KEY-N}/state           # workflow state name
/linear/teams/{KEY}/issues/{filter}/{KEY-N}/priority        # Urgent | High | Medium | Low | No priority
/linear/teams/{KEY}/issues/{filter}/{KEY-N}/assignee        # display name (empty if unassigned)
/linear/teams/{KEY}/issues/{filter}/{KEY-N}/description.md  # markdown body
```

Each issue file declares `Stability::Mutable` with `version=updatedAt`,
so the host can reuse cached content across opens until Linear's
`updatedAt` advances.

## Setup

The provider is built into the omnifs container image. Add a mount
config at `docker/providers/linear.json` (already provided) and supply
a Linear personal access token via the `LINEAR_TOKEN` env var.

### Token

Linear personal access tokens (`lin_api_...`) go in the `Authorization`
header without the `Bearer ` prefix. The mount config uses
`api-key-header` auth pointing at the `Authorization` header:

```json
{
  "plugin": "omnifs_provider_linear.wasm",
  "mount": "linear",
  "auth": {
    "type": "api-key-header",
    "header": "Authorization",
    "domain": "api.linear.app",
    "token_env": "LINEAR_TOKEN"
  },
  "capabilities": {
    "domains": ["api.linear.app"],
    "max_memory_mb": 128
  }
}
```

### Running in Docker

The repo's `just dev` recipe runs the default compose service. To run a
dedicated container for the Linear provider (alongside or instead of
the default), use the standalone wrapper:

```bash
docker build -t omnifs-linear:dev .
LINEAR_TOKEN=lin_api_... ./scripts/run-linear-container.sh
docker exec omnifs-linear ls /omnifs/linear/teams
```

The wrapper accepts optional `image` and `container` positional
arguments and defaults to `omnifs-linear:dev` and `omnifs-linear` so it
does not collide with the default `omnifs` container.

## Implementation notes

The provider uses hand-written GraphQL queries plus `serde` response
structs (see `src/graphql.rs`). Cynic is a natural fit for a code-first
GraphQL client, but Linear's API rejects the full introspection query
with `Query too complex`, so we cannot bootstrap codegen from the live
schema. The three-query surface is small enough that hand-written
queries are simpler and easier to audit.

Issue listings preload the per-issue inline files (`title`, `state`,
`priority`, `assignee`, and the description if it fits in 4 KiB) into
the response's projection map. A `cat` after an `ls` is served from
cache without an additional Linear round trip. Descriptions over 4 KiB
fall back to a deferred `#[file]` handler that fetches the issue body
on demand.

The polling interval (`refresh_interval_secs`) is 120 s. With Linear's
3M-point-per-hour API key budget, a few-team workspace stays well
under budget even with periodic refresh.

## TODOs (deferred from v1)

- Comments. Linear issues carry threaded comments; expose them under
  `comments/{n}` analogously to the GitHub provider.
- Cycles. `/linear/teams/{KEY}/cycles/{id}/issues/` would surface a
  cycle's contents.
- Projects. `/linear/projects/{slug}/issues/` (and per-team project
  links).
- Labels. Filter by label, or expose label sets as directories.
- Pagination cursors. The current implementation flattens all pages
  into one listing (capped at 2000 issues). Larger workspaces would
  want pagination via `PageStatus::More(Cursor::Opaque(end_cursor))`.
- OAuth tokens. The current auth wiring covers personal access tokens
  only; OAuth tokens would need the `Bearer ` prefix.
- `on-event` invalidation. Linear has webhooks but the provider does
  not subscribe; cache invalidation today is purely capacity-based.
