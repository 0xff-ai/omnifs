# Cache and effects

Status: current-architecture
Scope: why the host cache is a durable projection of provider terminals, how
complete facts serve without a provider, and why effects share one publication
boundary with typed results.

Read when: changing projection identity, durable cache facts, body storage,
publication, invalidation, cache-only serving, blob handles, or Git handoff.

Binding contracts: `docs/contracts/10-system.md`,
`docs/contracts/20-provider-sdk.md`, and
`docs/contracts/30-projection-tree.md`.

The host stores validated paths, attrs, bytes, opaque ids, listings, Git
identities, and freshness without interpreting provider objects.

## Durable owners

| Owner | Lifetime | Contents |
|---|---|---|
| `BodyStore` | Global durable | Complete inline, canonical, and blob bodies by BLAKE3 |
| `ProjectionStore` | Exact spec and provider identity | Relations, lookup/attr/file/listing facts, negatives, freshness, blob references, Git identities |
| `MountResources` | Candidate serving generation | Derived memory, opaque handles, reservations, invalidation epoch |

Bodies publish before projection rows reference them. Runtime handles, absolute
checkout paths, and invalidation epochs never enter durable storage.

Candidates for one projection share its durable facts and `ResourceFence` but
keep isolated `MountResources`. Activation advances the fence; prepared or
retired generations may finish work but cannot publish.

## Publication

A provider terminal lowers its typed result and effects into one
`ProjectionTransition`:

1. publish immutable bodies;
2. fence on active generation and captured invalidation epoch;
3. commit relations, facts, freshness, and invalidations with `SyncAll`;
4. update or evict memory, emit events, then expose the result.

Each committed object or listing invalidation advances the epoch once, so an
older terminal cannot republish stale facts. The transaction keeps aliases,
negative rows, complete listings, exact lookups, and Git facts consistent.
Malformed keys, dangling bodies, or inconsistent relations are corruption, not
cache misses.

## Online and cache-only reads

One `MountTable` feeds one `TreeNamespace` in both modes:

| Fact | Online | Cache-only |
|---|---|---|
| Fresh durable fact | Reuse | Reuse |
| Expired durable fact | Revalidate | Reuse |
| Exact positive or negative lookup | Answer | Answer |
| Complete listing, unknown child | `NotFound` | `NotFound` |
| Partial listing | Return known entries; provider may continue | Return known entries without controls |
| Missing body, live/ranged value, continuation | Use provider | `OfflineMiss` |

Online reuse of dynamic bodies still needs stable or versioned observation
identity. Cache-only entries have no provider `Runtime`.

Offline open requires the exact manifest, body root, database, and keyspace. It
creates or repairs no Omnifs state and never falls back to another identity.
Fjall may still recover its own journal while opening the existing database.

## Git handoff

Durable Git facts store a `GitId` and validated relative path. `GitId` binds
mount scope, canonical remote, and reference. Offline open validates the
existing private clone and capability confinement without Git or network I/O.
Process-local tree identity is `(GitId, relative path)`.

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
