---
title: The cache trilogy
description: The three host caches, what each stores, and how freshness works without TTLs.
---

The host owns all caching as plain byte storage. It does not reason about what an object means. It stores bytes and serves them back. There are three caches, each with a role.

The object cache is the durable primary. It holds canonical upstream bytes, keyed by logical id, in `object.redb`. It is what accumulates as you touch the world: once a resource's canonical bytes are stored, the host can re-render its files without going upstream again. Only object-shaped providers fill it. A provider that emits no canonical bytes opts out.

The view cache is derived and disposable. It holds the rendered files, fields, and directory listings your tools actually read, in `view.redb`, and it is deleted and recreated on every startup. Nothing of value is lost when it is empty, because everything in it recomputes from the object cache with no upstream call.

The blob cache holds large binary content on the host and serves it by handle, for example a PDF or an extracted archive. The bytes never pass through a provider.

Freshness is not time-based. There are no TTLs. Entries leave by capacity eviction or by explicit invalidation: a provider invalidates an object or a listing, and a per-mount generation fence makes sure a write derived from data read before that invalidation cannot land stale. Overwriting an object evicts the files derived from its previous version, so a reader never sees a field from one version beside a field from another.
