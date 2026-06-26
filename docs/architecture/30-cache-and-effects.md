# Cache and effects

Status: current-architecture
Scope: why the host cache is byte-oriented, why canonical bytes are durable, and why effects are the only terminal host-mutation channel. Binding rules live in `docs/contracts/10-system.md`, `docs/contracts/20-provider-sdk.md`, and `docs/contracts/30-projection-tree.md`.

The host owns storage, but not provider meaning. It stores paths, attrs, bytes, ids, and cache metadata as opaque facts. It does not parse provider objects or render representations.

## Cache roles

There are three host cache roles:

| Cache | Role | Durability |
|---|---|---|
| object | canonical upstream or provider-assembled bytes for object-shaped resources | durable |
| view | derived representations, direct file materializations, dirents, and learned attrs | disposable |
| blob | large host-resident binary content by handle | durable runtime storage |

The object cache is the durable primary for object-shaped data. The view cache is derived and can be rebuilt from object bytes or provider reads. The blob cache is for large bytes that should not cross into provider memory.

Structural providers can self-select out of the object cache by emitting no canonical stores. There is no host flag that says a provider is object-shaped.

## Warm object reads

On a warm object read, the host finds the canonical by exact path-to-id map lookup and pushes the cached canonical bytes into the provider read operation.

The provider must self-check the pushed id against the route-derived id. A wrong or stale map entry degrades to a refetch, not silently wrong bytes.

There is no `canonical-read` callout. The provider does not pull cached canonical bytes from the host. The host pushes them into the one read path.

## Effects

A provider step either suspends on callouts or returns a terminal result. Terminal host mutation happens only through effects:

- canonical effects store canonical bytes and view-leaf associations
- filesystem effects publish materialized files, dirs, attrs, and byte sources into the view layer
- invalidation effects remove object or listing state

Errors must not carry effects. If a provider needs a new terminal mutation, add an explicit effect field. Do not tunnel host mutation through callouts.

## Object and view coherence

Object eviction removes the canonical and its exact indexed view leaves. It must use the reverse leaf set, not textual prefix deletion.

Identity representations are not double-stored. Canonical bytes live in the object cache. Rendered representations and direct file materializations live in the view cache.

View data can carry freshness deadlines derived from stability. Object cache entries do not have provider TTLs. They leave by capacity eviction or explicit invalidation.

## Fences

Operations that render from a pushed canonical capture a generation before the push. Writes derived from that pushed canonical are admitted only if no newer invalidation tombstone covers the object.

The generation and tombstones are per-mount and runtime-only. They protect in-flight operations and disposable view writes. They do not need to survive restart because stale view data does not survive restart.

## Mount isolation

Object ids are mount-scoped by the host. Two mounts with different credentials must not share canonical bytes simply because a provider computed the same logical id.

The provider computes logical identity. The host supplies mount scoping and storage isolation.

## Rationale

Canonical bytes are the expensive part. Rendering alternate leaves from a canonical is cheap and local. Making the canonical durable lets a view miss after restart render without an upstream call.

Keeping effects as the single mutation channel makes provider behavior auditable. The host can validate, fence, and apply effects consistently instead of finding mutation hidden inside transport calls.

## Rejected shapes

- host-side object parsing or rendering
- provider-owned content LRUs or TTLs
- `canonical-read` callouts
- prefix scans for object leaf eviction
- durable view cache that can disagree with canonical bytes after restart
- errors that also mutate host state
