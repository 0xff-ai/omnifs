---
title: Reaching upstream
description: "The provider-authoring view of callouts and effects: how to suspend with a callout, await host results, and return canonical bytes and invalidations."
---

Providers do not perform direct network, Git, archive, or blob I/O. They request host work through callouts and return host mutations as effects.

## Callout flow

1. The host calls a provider operation.
2. The provider returns `suspended(list<callout>)`.
3. The host checks capabilities and executes the callouts.
4. The host calls `resume` with callout results.
5. The provider returns the final result and effects.

Callouts are request/response. There are no fire-and-forget callouts, and a returned provider step cannot carry trailing callouts.

## Callout types

| Callout | Use |
|---|---|
| `fetch` | HTTP request whose response body crosses WIT back to the provider. |
| `git-open-repo` | Open a Git repository and return a host tree handle. |
| `fetch-blob` | Fetch large bytes into the host blob cache; the provider receives a handle. |
| `open-archive` | Surface a stored blob as a navigable archive tree. |
| `read-blob` | Bring a bounded byte range from a blob handle into the provider. |

The SDK wraps these into async callout futures. In practice, you await a callout and the SDK manages the suspend/resume cycle.

## Effects

Effects are committed at the return boundary.

| Effect | Use |
|---|---|
| `canonical` | Store raw upstream object bytes under a logical object id. |
| `fs` | Write derived files or directories into the view cache. |
| `invalidations` | Evict cached objects or listings. |

Use `canonical` effects when a provider has loaded an object that can render several leaves. Supply the full set of view-leaf paths in the `canonical` effect so the host can evict them precisely when the object is later overwritten.

Use `fs` effects for materialized files and directories that should appear in the view cache directly.

Use invalidations when an event or operation proves cached data is stale. Invalidations come in two forms: evict a specific object by id, or evict all cached listings under a path prefix.

## Authority boundary

The provider asks for an HTTP fetch, Git handoff, blob read, archive open, or cache write. The host checks capabilities against the provider manifest and performs the work. That is the core rule.
