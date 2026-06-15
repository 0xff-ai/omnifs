---
title: "Cache semantics"
description: "Object cache, view cache, blob cache, invalidation, warm reads, and freshness caveats."
---

omnifs has three host-owned caches.

| Cache | Role | Persistence |
|---|---|---|
| Object cache | Canonical upstream bytes keyed by logical object id. | Durable |
| View cache | Derived files, directories, attributes, and listings. | Recreated on startup |
| Blob cache | Large host-side byte payloads and archive trees. | Host disk |

## Warm read

On a view miss under a known object path, the host can map the path to a logical id, load cached canonical bytes, and pass `canonical-input` to `read-file`. The SDK renders only if the id matches the route-derived id.

## Invalidation

Providers can return invalidation effects for objects or listings. The host also fences writes derived from stale pre-invalidation data.

## Freshness

The object cache is canonical storage; view records are disposable rendered records with attributes and freshness behavior. The caches are not TTL-based.
