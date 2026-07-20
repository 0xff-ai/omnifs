# Projection tree contracts

Status: current-contract
Owns: shared projection semantics, file attributes, cache ownership, listing, lookup, learned sizes, and live-file behavior.

## Read when

Read this before touching `omnifs-engine/src/tree`, projected node resolution, cache access, attrs, listing, lookup, traversal, learned sizes, negative lookup behavior, live growth, or behavior shared by FUSE and NFS.

## Rules

### TreeNamespace owns projection semantics

`omnifs-engine::namespace::TreeNamespace` is the sole public semantic facade. Its internal tree implementation owns answers to what projected path exists, what bytes or attrs it has, what cache entry should be published, what root children exist, and what provider probe is needed.

Put shared projection semantics behind `TreeNamespace`. Move behavior out of FUSE and NFS when it becomes frontend-neutral. Keep mount-root enumeration and host traversal in the namespace owner.

### File attributes

One file contract owns file truth per path. Size, stability, read mode, content type, byte source, and version evidence must stay together.

Learned sizes and read semantics belong with file attrs and tree policy, not frontend-local heuristics. Preserve learned-size rules in shared code and test tools that observe attrs and reads differently.

### Cache ownership

The host owns all caching as opaque byte storage. Providers do not add private LRUs or time-based expiration policy. Frontends do not own cache schema.

The namespace boundary carries engine-issued cache lifetimes. A frontend may retain only the plain positive or negative answer for that lifetime and must evict it on namespace invalidation. Missing children are lookup answers, while transport, offline, and provider failures remain errors.

`Caches` owns one Fjall database and one global content-addressed `BodyStore`. One `ProjectionStore` keyspace, selected by the exact mount-spec bytes and pinned provider identity, owns every durable fact for that immutable projection. The process-local memory tier is derived and belongs to the projection's `MountResources`.

Exactly one live `Arc<MountResources>` exists per projection identity in a process. Its single transition boundary publishes object relations, typed lookup/attr/file/listing facts, blob and Git references, freshness, and invalidations in one durable transaction. A provider terminal is observable only after that transaction commits and the derived memory tier is invalidated. Runtime invalidation epochs remain process-local and fence the complete stale transition.

Online and cache-only serving use the same `TreeNamespace` and fixed `MountTable`. Cache-only entries have no provider runtime. Complete durable facts remain usable regardless of online freshness, and partial durable listings return their known entries with no provider-dependent continuation; missing bodies or facts still return terminal `OfflineMiss`, while corruption aborts construction of the whole table.

Access-driven revalidation is the cache freshness boundary. An expired indexed view leaf makes the next read enter `read-file` with the cached canonical id, validator, and bytes in `ReadMode::Revalidate`, so normal provider effects apply refreshed canonical bytes or invalidations. Provider-declared timer events remain independent and use the manifest refresh interval.

### Listing and lookup

Lookup, listing, and read must use one shared target-resolution model. Listing must be honest about what is currently knowable without inventing provider-specific frontend behavior.

At each mount root, `.gitignore`, `.ignore`, and `.rgignore` are host-owned
synthetic regular files. Root lookup, listing, and read must agree on that
file kind and fixed content even when a provider or cached dirent projects a
colliding entry; below the mount root, providers may project those names
normally.

An offline partial listing is an honest snapshot of known children, not proof
that unknown children are absent. It must terminate locally without pagination
controls or provider continuations; lookup of a cached child succeeds, an
explicit cached negative remains `NotFound`, and an uncached child remains
`OfflineMiss`.

Keep ordered route precedence in one dispatcher. Durable definitive negative lookups are exact projection facts and must remain coherent with parent listings and object invalidation. Verify parent-directory traversal, not only intended leaf reads.

### Live growth

Follow-mode reads, growing sizes, EOF discovery, and invalidation for live files need one shared owner. Frontend pumps may deliver protocol mechanics, but not semantic rules for file growth, EOF learning, or cached attrs.

## Must not

- Let FUSE and NFS rediscover provider policy independently.
- Let frontends build projection cache keys or match on cache payload schema.
- Add per-frontend negative lookup policy, dotfile exceptions, or lookup suppression lists.
- Add parallel provider-facing and wire-facing file structs that can disagree.
- Reintroduce placeholder sizes for unknown-length files.
- Let a frontend decide whether a learned size is authoritative.
- Add provider-local caches for canonical object bytes.
- Duplicate dispatch ordering in list and lookup paths.
- Let static route scaffolding bind as dynamic captures.

## Code

- `crates/omnifs-engine/src/tree`
- `crates/omnifs-engine/src/runtime`
- `crates/omnifs-engine/src/cache`
- `crates/omnifs-sdk/src/router`
- `crates/omnifs-fuse/src`
- `crates/omnifs-nfs/src`

## Validation

- Add cross-frontend or tree conformance tests for behavior shared by FUSE and NFS.
- Cache changes need cold and warm read tests, plus invalidation coverage when behavior changes.
- Route/lookup/listing changes need tests that hit lookup, list, and read for the same route surface, including cold and warm paths.
- Size-sensitive changes need stat/read checks and relevant real-tool behavior.
