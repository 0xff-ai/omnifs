# Projection tree contracts

Status: current-contract
Owns: shared projection semantics, file attributes, cache ownership, listing, lookup, learned sizes, and live-file behavior.

## Read when

Read this before touching `omnifs-engine/src/tree`, projected node resolution, cache access, attrs, listing, lookup, traversal, learned sizes, negative lookup behavior, live growth, or behavior shared by FUSE and NFS.

## Rules

### Tree owns projection semantics

`omnifs-engine/src/tree` owns answers to what projected node exists, what bytes or attrs it has, what cache entry should be published, what root children exist, and what provider probe is needed.

Put shared projection semantics behind `Tree`. Move behavior out of FUSE and NFS when it becomes frontend-neutral. Keep root enumeration as a tree operation.

### File attributes

One file contract owns file truth per path. Size, stability, read mode, content type, byte source, and version evidence must stay together.

Learned sizes and read semantics belong with file attrs and tree policy, not frontend-local heuristics. Preserve learned-size rules in shared code and test tools that observe attrs and reads differently.

### Cache ownership

The host owns all caching as opaque byte storage. Providers do not add private LRUs or time-based expiration policy. Frontends do not own cache schema.

Keep object cache durable and provider-scoped. Treat view cache as derived and disposable. Access cache schema through shared host/tree APIs.

Host revalidation is a cache safety backstop, not a second invalidation channel. `MountRuntimes` drives the per-mount timer, `Runtime::revalidate_recent_objects` chooses from the recent object-read set, and `Namespace::revalidate_file` re-enters `read-file` with the cached canonical id, validator, and bytes so normal provider effects apply any refreshed canonical bytes or invalidations.

### Listing and lookup

Lookup, listing, and read must use one shared target-resolution model. Listing must be honest about what is currently knowable without inventing provider-specific frontend behavior.

Keep ordered route precedence in one dispatcher. Model negative lookups once in tree or host policy if they become needed. Verify parent-directory traversal, not only intended leaf reads.

### Serving scopes

`ServingContext` owns any read-only scope applied to the served namespace. A worldview filters the mount set and may bind a mount to a mount-relative subtree prefix. That scope is projection semantics, so it belongs in `omnifs-engine`, not in FUSE, NFS, the daemon, or providers.

Scope enforcement must stay on the shared serving funnels. `split_mount_path` and mount enumeration decide which mounts exist, `runtime_for` rejects out-of-scope target paths before provider dispatch, and the tree uses `scope_child_resolution` and `scope_directory_child` to expose the synthetic ancestor directories that make a scoped subtree reachable by component-by-component lookup.

A mount omitted from the active scope does not exist. A scoped subtree serves the prefix itself and descendants normally, serves only synthetic read-only ancestor directories above the prefix, and returns NotFound for every other path. Resolve, list, read, and open must all fail closed for out-of-scope paths, including direct handle rehydration or guessed provider-relative paths.

`registry_runtime` may keep seeing all runtimes because invalidation is host bookkeeping, not serving.

### Live growth

Follow-mode reads, growing sizes, EOF discovery, and invalidation for live files need one shared owner. Frontend pumps may deliver protocol mechanics, but not semantic rules for file growth, EOF learning, or cached attrs.

## Must not

- Let FUSE and NFS rediscover provider policy independently.
- Let frontends build projection cache keys or match on cache payload schema.
- Add per-frontend negative lookup policy, dotfile exceptions, or lookup suppression lists.
- Let frontends or providers know about worldview scope rules.
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
- Serving-scope changes need tests that prove mount filtering, scoped subtree reachability through synthetic ancestors, and NotFound behavior across resolve, list, read, and open.
- Size-sensitive changes need stat/read checks and relevant real-tool behavior.
