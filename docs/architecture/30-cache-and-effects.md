# Cache and effects

Status: current-architecture
Scope: why the host cache is a durable projection of provider terminals, how complete facts serve without a provider, and why effects share one publication boundary with typed results. Binding rules live in `docs/contracts/10-system.md`, `docs/contracts/20-provider-sdk.md`, and `docs/contracts/30-projection-tree.md`.

The host owns storage, but not provider meaning. It stores validated paths, attrs, bytes, opaque ids, listings, Git identities, and freshness as facts. It does not parse provider objects or render representations.

## Durable owners

`Caches` opens one global Fjall database and one global append-only `BodyStore`. `BodyStore` publishes complete bodies by BLAKE3 identity before any projection row can reference them. Inline file bytes, canonical object bytes, and streamed blob bodies all use that store.

Each exact mount revision selects one `ProjectionStore` by `ProjectionId`, which hashes the exact mount-spec bytes and pinned provider identity. Its strict manifest records the mount, spec digest, and provider identity. One tagged fact keyspace owns object relations, typed lookup/attr/file/listing records, definitive negatives, expiry, blob request references, and Git identities. Runtime blob handles, runtime Git handles, absolute checkout paths, and invalidation epochs are never durable.

`MountResources` is the one live process owner for a selected projection. It owns the `ProjectionStore`, shared `BodyStore`, derived memory tier, runtime-only opaque handles, publication reservations, and invalidation epoch. `Caches` deduplicates it with a weak owner slot, so one projection cannot acquire independent coherence fences inside a process.

## Publication

A provider operation validates and lowers its effects plus its operation-specific typed result into one `ProjectionTransition`. Body publication happens first because bodies are immutable and may remain unreferenced after a fence. The short projection transaction then applies object and path relations, records, listing mutations, freshness, and invalidations with `SyncAll` durability. It updates or evicts the derived memory tier only after commit, emits events after that eviction, and exposes the typed operation result last.

The process-local invalidation epoch fences the complete transition. Every operation captures the epoch before reading cache-derived inputs. Any committed object or listing invalidation advances the epoch exactly once, and a terminal captured before that invalidation fails instead of republishing stale facts.

Publication also keeps durable relations converged. Forward and reverse object aliases agree, definitive negative reverse rows agree, complete parent listings cannot contradict exact child lookup facts, and stale Git facts leave with non-subtree lookup replacements. Startup treats malformed keys, corrupt rows, dangling bodies, or inconsistent relations as corruption rather than a cache miss.

## Online and cache-only reads

One fixed `MountTable` feeds one `TreeNamespace`. Online entries own a provider `Runtime`; cache-only entries own the same projection resources and no runtime. Durable-answer paths use `MountResources` in both modes, while provider execution, ranged reads, live values, and pagination continuation require the optional runtime.

Online access uses freshness deadlines to decide when to revalidate. Cache-only access ignores those deadlines because complete durable facts remain facts after their online TTL. Exact positive and definitive negative lookups answer normally. A complete listing proves an unknown child is absent; a partial listing cannot. Missing bodies, incomplete listings, deferred/live/ranged values, and any provider-dependent continuation return terminal `OfflineMiss`.

Offline construction is non-creating and non-repairing at the Omnifs semantic layer. It requires the exact projection manifest, body root, database, and fact keyspace and creates no Omnifs directories, manifests, keyspaces, rows, bodies, or temporary files. Fjall has no read-only recovery mode, so opening an existing database may perform storage-engine journal recovery under its exclusivity lock; that maintenance does not authorize semantic cache repair or fallback identity selection.

## Git handoff

Durable Git facts store a `GitId` plus one validated relative path. The Git id binds the mount scope, canonical remote, and reference. Offline open validates the existing private clone binding and capability confinement without a Git subprocess or network access, then constructs the host tree from the selected relative directory. Process-local tree identity is the pair `(GitId, relative path)`, so two subtrees from the same clone remain distinct while the same selection deduplicates.

## Rejected shapes

- host-side object parsing or rendering
- provider-owned content LRUs or TTLs
- separate durable object, view, or blob stores
- per-mount body stores or mount-name projection selection
- errors that also mutate host state
- split result/effect publication
- persisted runtime handles, absolute host paths, or invalidation generations
- fallback projections, current pointers, legacy readers, or cache repair during offline open
- fake runtimes or a second namespace implementation for cache-only serving
