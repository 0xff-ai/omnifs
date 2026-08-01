# Projection tree contracts

Status: current-contract
Owns: projection semantics, attrs, cache ownership, listing, lookup, learned
sizes, and live files.

## Read when

Read this before changing the engine tree, node resolution, cache access,
attrs, listing, lookup, traversal, learned size, live growth, or behavior shared
by FUSE and NFS.

## Rules

### TreeNamespace owns projection semantics

`TreeNamespace` is the sole public semantic facade. Its tree owns path
existence, bytes, attrs, cache publication, root children, and provider probes.
Move filesystem-neutral behavior out of FUSE and NFS into this owner.

### File attributes

One file contract keeps size, stability, read mode, content type, byte source,
and version evidence together. Learned size and read semantics stay in shared
tree policy, with tests for tools that observe attrs and reads differently.

### Cache ownership

The host caches opaque bytes and facts. Providers add no private LRU or TTL
policy; filesystems own no cache schema.

The namespace boundary issues cache lifetimes. Filesystems may retain only the
plain positive or negative answer for that lifetime and evict on invalidation.
Missing children are lookup answers; transport, offline, and provider failures
remain errors.

`Caches` owns one Fjall database and global content-addressed `BodyStore`. Exact
mount-spec bytes plus pinned provider identity select the `ProjectionStore`
that owns every durable projection fact. `MountResources` owns derived memory.

Each prepared serving generation owns isolated process-local `MountResources`
over the shared durable projection and `BodyStore`. All live resources for one
projection identity share one `ResourceFence`. Activation advances that fence;
only the active generation may publish, and retirement prevents the old
generation from publishing later. Each generation keeps its own memory tier,
runtime handles, reservations, and invalidation epoch. Its transition boundary
publishes object relations, typed lookup/attr/file/listing facts, blob and Git
references, freshness, and invalidations in one durable transaction. A
provider terminal is observable only after that transaction commits and the
derived memory tier is invalidated.

Online and cache-only serving share `TreeNamespace` and `MountTable`.
Cache-only entries have no provider runtime. They ignore online freshness,
return known entries from partial listings without continuation, and return
`OfflineMiss` for missing bodies or facts. Corruption aborts the whole table.

On access, an expired indexed view leaf enters `read-file` in
`ReadMode::Revalidate` with cached canonical ID, validator, and bytes. Normal
effects publish refresh or invalidation. Provider timer events remain
independent and use the manifest interval.

### Listing and lookup

Lookup, listing, and read share one target-resolution model. Listing reports
only what is currently knowable.

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

One dispatcher owns route precedence. Durable definitive negatives stay
coherent with parent listings and object invalidation. Verify parent traversal,
not only leaf reads.

### Live growth

Shared tree policy owns follow reads, growth, EOF discovery, invalidation, and
cached attrs. Filesystem pumps own only protocol delivery.

## Must not

- Let FUSE and NFS rediscover provider policy independently.
- Let filesystems build projection cache keys or match on cache payload schema.
- Add per-filesystem negative lookup policy, dotfile exceptions, or lookup suppression lists.
- Add parallel provider-facing and wire-facing file structs that can disagree.
- Report a guessed exact size for an unknown-length file or use the non-zero
  stat sentinel as a read bound.
- Let a filesystem decide whether a learned size is authoritative.
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

- Add cross-filesystem or tree conformance tests for behavior shared by FUSE and NFS.
- Cache changes need cold and warm read tests, plus invalidation coverage when behavior changes.
- Route, lookup, or listing changes need tests that hit lookup, list, and read
  for the same route surface, including cold and warm paths.
- Size-sensitive changes need stat/read checks and relevant real-tool behavior.
