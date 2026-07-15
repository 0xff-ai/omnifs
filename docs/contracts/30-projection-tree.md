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

Keep object cache durable and provider-scoped. Treat view cache as derived and disposable. Access cache schema through shared host/tree APIs.

Each immutable running mount owns one `Arc<MountResources>`. That owner is the
authority for canonical bytes, forward and reverse object indexes, negative
state, generation fences, view admission, and mount blob storage. Durable
publication and invalidation transitions use its short synchronous boundary;
the boundary never spans provider execution, filesystem I/O, or an await.

Access-driven revalidation is the cache freshness boundary. An expired indexed view leaf makes the next read enter `read-file` with the cached canonical id, validator, and bytes in `ReadMode::Revalidate`, so normal provider effects apply refreshed canonical bytes or invalidations. Provider-declared timer events remain independent and use the manifest refresh interval.

### Listing and lookup

Lookup, listing, and read must use one shared target-resolution model. Listing must be honest about what is currently knowable without inventing provider-specific frontend behavior.

At each mount root, `.gitignore`, `.ignore`, and `.rgignore` are host-owned
synthetic regular files. Root lookup, listing, and read must agree on that
file kind and fixed content even when a provider or cached dirent projects a
colliding entry; below the mount root, providers may project those names
normally.

Keep ordered route precedence in one dispatcher. Model negative lookups once in tree or host policy if they become needed. Verify parent-directory traversal, not only intended leaf reads.

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
