# Caching model

## The rule

The host owns **all** caching. Providers must not reintroduce their own
LRUs, memoization, or time-based expiration.

## Shape

Two tiers, both in `crates/host/src/cache/`:

- **L0** — in-memory `moka` cache, per provider instance. ~32 MiB weight
  cap; payloads above ~256 KiB skip L0.
- **L2** — durable `redb`-backed cache. Bulk threshold ~64 KiB.

## TTLs: there are none

Eviction is by capacity or by **explicit invalidation**. There are no
time-based expirations. Two invalidation paths:

- `event-outcome` returned by a provider's `on-event` handler, with
  `invalidate-paths` and `invalidate-prefixes`. Applied by the host at
  the response boundary.
- The FUSE notifier path inside the host.

## Sibling and preload data folds into the terminal

Cache side effects do not travel as separate callouts. They ride inside the
terminal that produced them:

- `list-children` listings carry a `preload` field with content for named
  paths the host should cache alongside the listing.
- A `lookup-child` returning a directory through a `#[dir]` handler carries
  the same field on `lookup-entry.preload`.
- `read-file` carries `sibling-files` so adjacent file contents can be
  cached without another provider round trip.

## What this means for code changes

- Don't add a per-provider in-memory map for "things we just fetched".
  Use `Projection::preload` / `with_sibling_files` so the host caches it.
- Don't add a TTL to an existing cache entry. If something needs to be
  invalidated, route it through `event-outcome` or the FUSE notifier.
- Don't add a "force refresh" knob unless there's a concrete bug that
  invalidation can't solve.

See `docs/repo-intent.md` for the architectural commitment, and the
relevant design docs for protocol-level details.
