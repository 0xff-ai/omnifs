---
title: Callouts and effects
description: How the host authorizes and runs provider callouts, then commits canonical, fs, and invalidation effects at the return boundary.
---

Providers do not perform ambient I/O. Every network request, git open, blob fetch, and archive open is a callout: the provider suspends, the host checks capabilities and runs the work, and the host resumes the provider with results. Effects are the return channel for host mutations.

## The callout cycle

A provider operation returns one of two steps:

```text
suspended(list<callout>)
returned(provider-return)
```

`suspended` is not a result. It means the host must run the listed callouts and resume the same operation with matching callout results via `resume`. Callouts are strictly request/response: there are no fire-and-forget callouts, and a `returned` step cannot carry trailing callouts.

## Callout types

| Callout | What the host does |
|---|---|
| `fetch` | Executes an HTTP request; response body crosses WIT back to the provider. |
| `git-open-repo` | Opens a Git repository and returns a host-managed tree handle. |
| `fetch-blob` | Fetches large bytes into the host blob cache; the provider receives a handle. |
| `open-archive` | Surfaces a stored blob as a navigable archive tree. |
| `read-blob` | Brings a bounded byte range from a blob handle into the provider. |

For `fetch` callouts, the host injects auth headers where the provider manifest permits it. The provider receives response bytes, not the credential.

## Effects

Effects are committed at the return boundary when the provider step is `returned`. They are not background jobs.

| Effect block | Meaning |
|---|---|
| `canonical` | Store canonical upstream bytes under a logical object id. |
| `fs` | Write materialized files or directories into the view cache. |
| `invalidations` | Evict cached objects or listings. |

A `canonical` effect can carry `view-leaves`: the exact paths in the view cache that map to this object id. The host uses these to evict derived view entries when the object is later overwritten.

`invalidations` come in two forms: evict a specific object by id (`object`), or evict all cached listings under a path prefix (`listing`). A per-mount generation fence rejects any write derived from data read before a concurrent invalidation.

## Error kinds

Provider and callout errors share a common kind set: not found, not a directory, not a file, permission denied, invalid input, too large, network, timeout, denied, rate limited, version mismatch, and internal. `rate-limited` can carry a structured `retry-after` hint; do not bury retry policy in an error string.
