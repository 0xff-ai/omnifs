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

Online and cache-only serving use the same `TreeNamespace` and fixed `MountTable`. Cache-only entries have no provider runtime: a mount without its provider is one. Complete durable facts remain usable regardless of online freshness, and partial durable listings return their known entries with no provider-dependent continuation; missing bodies or facts still return terminal `OfflineMiss`, while corruption aborts construction of the whole table.

Access-driven revalidation is the cache freshness boundary. An expired indexed view leaf makes the next read enter `read-file` with the cached canonical id, validator, and bytes in `ReadMode::Revalidate`, so normal provider effects apply refreshed canonical bytes or invalidations. Provider-declared timer events remain independent and use the manifest refresh interval.

A cached listing keeps its pages, each with the cursor that asked for it. A page with a validator, complete or not, has the deadline of the interval of its upstream, else `DYNAMIC_TTL_MILLIS`. Before a cached listing serves online, each of its pages past its deadline is asked for again, in page order, with its cursor and validator: `unchanged` keeps the entries of the page and renews it with the validator of the answer, a changed page replaces its entries, keeps the later pages only while its next cursor stays, and records the directory as changed, and a failed answer, a rate limit included, is the error of the listing. Every `list-children` call carries the validator of the cached page of its cursor. A page without a validator keeps the freshness of a cached listing: a complete or paged listing serves until it is invalidated. While the rate-limit window of the mount is open, a cached listing serves without asking.

Fed facts come from feeds alone. The projection store keeps each feed object's token, whether its first answer ended, and its deadline, `DYNAMIC_TTL_MILLIS` after its last answer; an inner state a part brings is a token whose first answer ended. An operation on a path whose fact the projection lacks or is fed asks the provider, and a `feed(claims)` answer is served from the projection, or as not found, once the answering feed has answered: the outer claim whose first answer ended, else the innermost one. The facts of a fed object live under the anchor path of its claim, which has the facets of its route at their first choice, for every facet. A part names each anchor of another facet of an object that it writes with the anchor of the claim of the object; the projection keeps the pair, and the host serves the paths under that anchor and its listing as the same paths under the anchor of the claim, without the provider, while the lookup of the anchor stays at its own path, in the listing of its parent. Before the pair, an operation below an anchor of another facet has its fact at the same path under the anchor of its claim. A feed with kept parts of a later answer continues that answer from the token of its last kept part for an operation on one of its paths, whatever its deadline, and the operation takes the error of a failure; while the rate-limit window of the mount is open, the operation is served from the projection without that answer. Else an operation that starts that feed without its lookup, its file, or its complete listing in the projection takes an answer with its path under the anchor of the claim as `start`, and any other operation takes an answer once the feed is past its deadline, and not while the rate-limit window of the mount is open. The answering feed decides again in its flight, so an operation whose feed another answer moved meanwhile takes that answer. One answer runs in a flight per feed object and calls the export until a part without `more`. Each part of the first answer of a feed publishes with its effects, its token, and its inner states in one transition, and a failure keeps the last published token and is the error of each operation that waits for the answer. A part of a later answer, after the end of the first one, is kept with its effects, its token, and its inner states, durable in the projection store apart from its facts, and the part without `more` publishes the kept parts with it in one transition and drops them; a write of a later part keeps a path that an earlier part removes. A failure keeps the kept parts. The facts of a part have no deadline, answer filesystems with the zero lifetime of dynamic entries, and are left to their feeds by every other terminal. A `complete` listing write keeps its listing complete through the lookup reconciliation of its transition, a removal leaves its parent listing as complete as it was, and a part records the listings and paths it changes as changed: a changed listing reaches filesystems as its directory's attributes changed in place, so the directory and the nodes below it keep their identity, while a changed file or a removed path is invalidated. `projection.listing` answers only within a feed operation and for a path in the subtree of its object, from the projection alone, without the kept parts.

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
