---
title: Design docs & RFCs
description: Index of the in-repo design documents that are the source of truth behind these concept, provider, and SDK pages.
---

The pages in this site explain how omnifs works and how to use it. The **design
documents** they are based on live in the repository under `docs/design/` and
`docs/future/`. Those documents are the source of truth: when this site and a
design doc disagree, the design doc wins, and a change to dispatch, caching,
auth, or the protocol should land in the design doc first.

This page indexes them so every concept, provider, and SDK page has a canonical
"read the design" link.

## Protocol & runtime

| Document | Source | Site page |
| --- | --- | --- |
| Protocol paths — the single absolute path space | [`docs/design/protocol-paths.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/protocol-paths.md) | [The single path space](/concepts/path-space/) |
| Provider model — WIT interface, free-function handlers | [`docs/design/protocol-provider-model.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/protocol-provider-model.md) | [Provider model](/concepts/provider-model/) |
| Protocol shape — callouts, terminals, resume | [`docs/design/protocol-shape.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/protocol-shape.md) | [Callout runtime](/concepts/callout-runtime/) |
| Path dispatch & listing — routing precedence, exhaustiveness | [`docs/design/path-dispatch-and-listing.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/path-dispatch-and-listing.md) | [Path dispatch & listing](/concepts/path-dispatch/) |
| The `omnifs:provider` WIT interface | [`wit/provider.wit`](https://github.com/0xff-ai/omnifs/blob/main/wit/provider.wit) | [WIT reference](/building-providers/wit-reference/) |

## Caching, files & storage

| Document | Source | Site page |
| --- | --- | --- |
| Cache architecture — host-owned, capacity-bounded, no TTLs | [`docs/design/cache-architecture.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/cache-architecture.md) | [Caching](/concepts/caching/) |
| File attributes — Size/Bytes/ReadMode/Stability | [`docs/design/file-attributes.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/file-attributes.md) | [File attributes](/concepts/file-attributes/) |
| Projected file sizes — learned-size promotion | [`docs/design/projected-file-sizes.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/projected-file-sizes.md) | [File attributes](/concepts/file-attributes/) |

## Auth, mounts & sandbox

| Document | Source | Site page |
| --- | --- | --- |
| Host auth — auth manifest, credential store | [`docs/design/host-auth.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/host-auth.md) | [Auth & credentials](/concepts/auth-credentials/) |
| OAuth client & flows | [`docs/oauth.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/oauth.md) | [Authenticating providers](/guides/authentication/) |
| Mount lifecycle & effective config | [`docs/design/mount-lifecycle.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/mount-lifecycle.md) | [Mount lifecycle](/concepts/mount-lifecycle/) |
| WASM sandbox substrate — Wasmtime/WASI plumbing | [`docs/design/wasm-sandbox-substrate.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/wasm-sandbox-substrate.md) | [WASM sandbox substrate](/concepts/wasm-sandbox/) |

## Providers

| Document | Source | Site page |
| --- | --- | --- |
| arXiv recent submissions | [`docs/design/arxiv-recent-submissions.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/arxiv-recent-submissions.md) | [arXiv provider](/providers/arxiv/) |
| Database provider | [`docs/design/providers/db.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/providers/db.md) | [Database provider](/providers/database/) |
| Linear provider | [`docs/design/providers/linear.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/providers/linear.md) | [Linear provider](/providers/linear/) |

## CLI, distribution & observability

| Document | Source | Site page |
| --- | --- | --- |
| CLI redesign — command surface | [`docs/design/cli-redesign.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/cli-redesign.md) | [CLI reference](/cli/) |
| npm distribution — bin shim, platform packages | [`docs/design/npm-distribution.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/npm-distribution.md) | [npm packaging](/releasing/npm/) |
| Inspector emission architecture — event stream | [`docs/design/inspector-emission-architecture.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/design/inspector-emission-architecture.md) | [Observability](/contributing/observability/) |

## Future / proposed

These describe work that is not implemented yet. They set the intended contract
before code exists; see [Future & RFCs](/reference/future/) for the reader-facing
summary.

| Document | Source |
| --- | --- |
| Async HTTP callouts | [`docs/future/async-http.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/future/async-http.md) |
| Mutations via git | [`docs/future/mutations-via-git.md`](https://github.com/0xff-ai/omnifs/blob/main/docs/future/mutations-via-git.md) |
