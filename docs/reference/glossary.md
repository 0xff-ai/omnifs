---
title: "Glossary"
description: "Reference glossary for omnifs runtime, provider, cache, security, and path terms."
---

| Term | Meaning |
|---|---|
| Path | The filesystem address a tool opens, lists, stats, or reads. |
| Mount | A configured provider instance under a mount prefix, such as `/github` or `/db`. |
| Provider | A `wasm32-wasip2` component that defines routes, object identity, and rendered files for one system. |
| Host | The runtime that owns FUSE, credentials, cache, callouts, provider lifecycle, and runtime grants. |
| WIT | The `omnifs:provider` interface between host and provider. |
| Lookup | The authoritative question: does this child name exist under this parent path? |
| List response | A provider response for directory listing. It may be exhaustive or non-exhaustive. |
| Cursor | Pagination state for listing responses. |
| Callout | Host work requested by a suspended provider operation, such as HTTP fetch, git handoff, blob fetch, or archive access. |
| Effect | Terminal provider output that asks the host to store canonical bytes, materialize view records, or invalidate cached state. |
| Object | A logical upstream resource represented by one or more rendered files. |
| Logical id | The stable provider-defined identity for an object inside a mount. |
| Canonical bytes | Provider-supplied upstream bytes stored in the object cache. |
| Cached canonical | Canonical bytes pushed by the host back into `read-file` so the provider can render without refetching. |
| Field leaf | A small file derived from an object, such as `title`, `state`, or `assignee`. |
| View leaves | Rendered filesystem records associated with an object and stored in the view cache. |
| Rendered view | Files and directories shell tools read after provider rendering. |
| Object cache | Durable cache for canonical object bytes. |
| View cache | Recreated-on-startup cache for derived filesystem records. |
| Blob cache | Host-managed byte storage for large fetched blobs, archives, and streamed content. |
| Tree-ref | Provider handoff to a real backing tree, such as a cloned repository. |
| Capability | A manifest-declared permission for host-mediated effects such as domains, Git remotes, Unix sockets, or preopened paths. |
| WASI preopen | Explicit filesystem path granted to the provider sandbox. |
| No ambient authority | Providers have no direct network, arbitrary disk, Docker, Git, or credential access outside host-mediated capabilities. |

