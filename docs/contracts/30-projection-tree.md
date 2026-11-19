# Projection tree contracts

Status: current-contract
Owns: shared projection semantics, file attributes, cache ownership, listing, lookup, learned sizes, and live-file behavior.

## Read when

Read this before touching `omnifs-engine/src/tree`, projected node resolution, cache access, attrs, listing, lookup, traversal, learned sizes, negative lookup behavior, live growth, or behavior shared by FUSE and NFS.

## Rules

### TreeNamespace owns projection semantics

`omnifs-engine::namespace::TreeNamespace` is the sole public semantic facade. Its internal tree implementation owns answers to what projected path exists, what bytes or attrs it has, what cache entry should be published, what root children exist, and what provider probe is needed.

Put shared projection semantics behind `TreeNamespace`. Move behavior out of FUSE and NFS when it becomes filesystem-neutral. Keep mount-root enumeration and host traversal in the namespace owner.

### File attributes

One file contract owns file truth per path. Size, stability, read mode, content type, byte source, and version evidence must stay together.

Learned sizes and read semantics belong with file attrs and tree policy, not filesystem-local heuristics. Preserve learned-size rules in shared code and test tools that observe attrs and reads differently.

### Cache ownership

The host owns all caching as opaque byte storage. Providers do not add private LRUs or time-based expiration policy. Filesystems do not own cache schema.

The namespace boundary carries engine-issued cache lifetimes. A filesystem may retain only the plain positive or negative answer for that lifetime and must evict it on namespace invalidation. Missing children are lookup answers, while transport, offline, and provider failures remain errors.

`Caches` owns one Fjall database and one global content-addressed `BodyStore`. One `ProjectionStore` keyspace, selected by the exact mount-spec bytes and pinned provider identity, owns every durable fact for that immutable projection. The process-local memory tier is derived and belongs to the projection's `MountResources`.

Exactly one live `Arc<MountResources>` exists per projection identity in a process. Its single transition boundary publishes object relations, typed lookup/attr/file/listing facts, blob and Git references, freshness, and invalidations in one durable transaction. A provider terminal is observable only after that transaction commits and the derived memory tier is invalidated. Runtime invalidation epochs remain process-local and fence the complete stale transition.

Online and cache-only serving use the same `TreeNamespace` and fixed `MountTable`. Cache-only entries have no provider runtime. Complete durable facts remain usable regardless of online freshness, and partial durable listings return their known entries with no provider-dependent continuation; missing bodies or facts still return terminal `OfflineMiss`, while corruption aborts construction of the whole table.

Access-driven revalidation is the cache freshness boundary. An expired indexed view leaf makes the next read enter `read-file` with the cached canonical id, validator, and bytes in `ReadMode::Revalidate`, so normal provider effects apply refreshed canonical bytes or invalidations. Provider-declared timer events remain independent and use the manifest refresh interval.

A cached listing keeps its pages, each with the cursor that asked for it. A page with a validator, complete or not, has the deadline of the interval of its upstream, else `DYNAMIC_TTL_MILLIS`. Before a cached listing serves online, each of its pages past its deadline is asked for again, in page order, with its cursor and validator: `unchanged` keeps the entries of the page and renews its deadline, and a changed page replaces its entries, keeps the later pages only while its next cursor stays, and records the directory as changed. Every `list-children` call carries the validator of the cached page of its cursor. A page without a validator keeps the freshness of a cached listing: a complete or paged listing serves until it is invalidated. While the rate-limit window of the mount is open, a cached listing serves without asking.

A Git tree keeps the commit of its last handoff in the Git fact of its tree path, with the deadline of `DYNAMIC_TTL_MILLIS` after each answer for it. Before the root of a tree or a node of it is served online past that deadline, `list-children` of the tree path asks again with the commit as its validator, in the one flight of the path and not while the rate-limit window of the mount is open: `unchanged` renews the deadline, and a subtree with a new commit updates the Git fact, renews it, and records the tree path as changed, so that filesystems see the attributes of the root of the tree changed in place. The nodes of a tree answer filesystems with the zero lifetime of dynamic entries, so each access reaches the namespace. A Git fact without an open tree, as after a restart, is a lookup miss, and the provider opens the repository again.

Fed facts come from feeds alone. The projection store keeps each feed object's token, whether its first answer ended, and its deadline, `DYNAMIC_TTL_MILLIS` after its last answer; an inner state a part brings is a token whose first answer ended. An operation on a path whose fact the projection lacks or is fed asks the provider, and a `feed(claims)` answer is served from the projection, or as not found, once the answering feed has answered: the outer claim whose first answer ended, else the innermost one. An operation that starts that feed without its lookup, its file, or its complete listing in the projection takes an answer with its path as `start`; any other operation takes an answer once the feed is past its deadline, and not while the rate-limit window of the mount is open. One answer runs in a flight per feed object, calls the export until a part without `more`, and publishes each part with its effects, its token, and its inner states in one transition; a failure keeps the last published token and is the error of each operation that waits for the answer. The facts of a part have no deadline, answer filesystems with the zero lifetime of dynamic entries, and are left to their feeds by every other terminal. A `complete` listing write keeps its listing complete through the lookup reconciliation of its transition, a removal leaves its parent listing as complete as it was, and a part records the listings and paths it changes as changed: a changed listing reaches filesystems as its directory's attributes changed in place, so the directory and the nodes below it keep their identity, while a changed file or a removed path is invalidated. `projection.listing` answers only within a feed operation and for a path in the subtree of its object.

### Listing and lookup

Lookup, listing, and read must use one shared target-resolution model. Listing must be honest about what is currently knowable without inventing provider-specific filesystem behavior.

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

A listing, or the siblings of a lookup, names a directory child without its own lookup, so it gives the child no lookup fact, and an online lookup of a listed directory asks the provider: only that answer tells a provider directory from the handoff of a Git tree, whose children the host resolves in the tree.

### Live growth

Follow-mode reads, growing sizes, EOF discovery, and invalidation for live files need one shared owner. Filesystem pumps may deliver protocol mechanics, but not semantic rules for file growth, EOF learning, or cached attrs.

## Must not

- Let FUSE and NFS rediscover provider policy independently.
- Let filesystems build projection cache keys or match on cache payload schema.
- Add per-filesystem negative lookup policy, dotfile exceptions, or lookup suppression lists.
- Add parallel provider-facing and wire-facing file structs that can disagree.
- Reintroduce placeholder sizes for unknown-length files.
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
- Route/lookup/listing changes need tests that hit lookup, list, and read for the same route surface, including cold and warm paths.
- Size-sensitive changes need stat/read checks and relevant real-tool behavior.
