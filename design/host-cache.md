# Persistent host-side cache

Status: draft, research + proposal
Scope: `crates/host/src/cache/`, `crates/host/src/runtime/`, FUSE notifier glue, CLI `--cache-dir`, mount registry, no WIT changes
Branch: `claude/host-cache-design-Z9Wyn`

## Goal

Pin down the host's persistent cache as a deliberate component rather than
"redb with three tables that grew organically." We want a cache that:

1. survives process restarts without serving stale data,
2. has bounded on-disk footprint per mount, with a sane eviction policy,
3. distinguishes metadata from content so big blobs don't crowd out hot
   listings,
4. handles invalidation cheaply at the prefix and path granularity the
   protocol already speaks,
5. has clear semantics around freshness, schema migration, crash safety,
   and concurrent access,
6. stays embeddable, single-process, pure Rust, and avoids RocksDB-class
   dependencies unless it earns its keep.

This document surveys the design space, names the concerns, evaluates
candidate tech stacks, and proposes a concrete architecture with a small
set of executable prototypes.

## Current state

The host already has a two-tier browse cache, in
`crates/host/src/cache/{l0,l2}.rs`:

- **L0** `BrowseCacheL0` (`l0.rs:20-57`). In-memory `moka::sync::Cache`,
  inode-keyed (`L0Key { inode, kind, aux }`), 32 MiB weight per provider
  instance, 256 KiB skip threshold. TinyLFU eviction (moka default).
- **L2** `BrowseCacheL2` (`l2.rs:14-123`). `redb::Database` per mount at
  `{cache_dir}/providers/{mount}/browse.redb`. Three tables: `metadata`,
  `content` (<64 KiB files), `bulk` (>=64 KiB files). Path-keyed
  (`{kind_char}:{path}`). No TTL, no quota, no capacity-driven eviction.
  Entries leave only via `delete_exact` / `delete_prefix`.

Invalidation flows from provider `event-outcome.invalidate-paths` /
`invalidate-prefixes` into `cache_delete_path` / `cache_delete_prefix`
(`runtime/invalidation.rs:71-143`), which fan out to: (a) the activity
table, (b) the L2 redb, (c) the kernel via `notifier.inval_entry` on
Linux. L0 entries are evicted lazily by `drain_and_evict_l0` on the next
FUSE op (`fuse/mod.rs:182-289`).

Preloads land at the response boundary in `apply_preloads`
(`runtime/mod.rs:333-394`) via `cache_put_batch`.

What's missing today:

- L2 grows unbounded. There is no per-mount quota, no global quota, no
  eviction policy, no GC.
- No notion of "freshness": a record either exists or it doesn't.
  The protocol has no validator (ETag / version / generation).
- No schema migration story beyond `SCHEMA_VERSION: u8` returning `None`
  (cache miss) on mismatch, which silently loses everything on a bump.
- No deduplication of file content: identical blobs at different paths
  are stored twice.
- No crash-consistency story between L0 and L2 (L0 happily serves a
  record that L2 already deleted but FUSE hasn't drained yet).
- No coordination if a second host process opens the same DB (redb
  errors out on the second `Database::create`, but we don't surface
  that as a useful diagnostic).
- L0 is per-mount and sized per-instance, so total memory scales with
  mount count without a global cap.

## Workload model

The browse cache sees four record kinds with different access patterns
and storage characteristics. Designing the persistent layer without
this in mind is how you end up with a single big LSM that thrashes:

| Kind        | Producer                       | Typical size | Read frequency             | Write pattern                       | Reuse on restart                    |
|-------------|--------------------------------|--------------|----------------------------|-------------------------------------|--------------------------------------|
| `Lookup`    | `lookup_child` + sibling proj. | 16-64 B      | Very high (every path op)  | Bursty (preload at terminal)        | High; topology changes slowly        |
| `Attr`      | `lookup_child` + dirents proj. | 16-64 B      | High (`getattr`)           | Coalesced with dirents              | High                                 |
| `Dirents`   | `list_children`                | 0.5-200 KiB  | Medium (readdir, exhaustive checks) | One write per dir + preloads | Medium; reorderings invalidate       |
| `File`      | `read_file`, sibling files     | 1 B - 100 MiB+ | Low to medium (per file)  | One write per file open             | Highly variable; large files dominate |

Two orthogonal observations:

- Metadata records are tiny but read at every FUSE op. They fit a small,
  contiguous, often-mmaped store. Writes are bursty (a single dir
  listing can produce tens to hundreds of records).
- File content has a bimodal size distribution. Below ~64 KiB it
  behaves like metadata; above ~1 MiB it behaves like a blob and wants
  content-addressed dedup, streamed reads, and separate eviction
  accounting.

This justifies splitting metadata and content into separate physical
stores even if they share a logical API.

## Design dimensions

### Scoping

What should the cache key space be partitioned by?

- **Mount** is non-negotiable. Each mount is a separate provider
  instance with its own auth, its own path universe, and its own
  invalidation rhythm. Co-mingling mounts breaks blast-radius bounds
  on `delete_prefix("/")`.
- **Provider plugin version**. A `wasm32-wasip2` component upgrade can
  reshape how a provider lays out its tree (e.g. GitHub adds a new
  pseudo-dir). Cached records from the previous version aren't
  necessarily wrong, but the safe default is to wipe per-mount L2
  on plugin hash change. Cheap insurance.
- **Provider config**. Two mounts pointed at the same provider with
  different auth / scope (e.g. different GitHub orgs) must not share
  cache. The mount ID already encodes this in our model, so this falls
  out of mount scoping.
- **Account / capability**. Within one mount, a provider may serve
  paths that depend on the caller's permissions. Today omnifs is
  single-user single-host, so we don't need per-principal scoping yet,
  but the key format should leave room (e.g. an optional `principal`
  prefix) to avoid a painful migration later.
- **Snapshot / generation**. For providers that expose immutable
  snapshots (a git ref, an S3 versioned object, a Notion page
  revision), the cache key wins from being snapshot-scoped: writes are
  monotonic, invalidation collapses to "drop old generations." This is
  a provider-protocol question more than a host-cache question, and
  the protocol doesn't speak generations today. Out of scope here.

**Recommendation:** the per-mount L2 instance stays. The cache_dir
layout becomes `{cache_dir}/providers/{mount}/{plugin_hash}/...` so a
provider upgrade simply switches subdirectories, and the previous
generation can be GC'd by a janitor at startup.

### Keying

The current key is `{kind_char}:{absolute_path_under_mount}`. That has
three deficiencies:

1. The kind char and the path live in the same string, which means a
   `delete_prefix("/")` has to scan four key spaces. A typed
   `(kind, path)` tuple key (redb supports composite keys via
   `Key`/`Value` impls) would let us scan one space per kind.
2. Path keys aren't normalized. A provider that emits both `/foo/bar`
   and `/foo//bar` will populate two records that the host will
   silently disagree about. The runtime should normalize once at the
   boundary.
3. There is no separation between "logical key" and "value identity."
   For file content, it would be cheaper to key the content table by a
   content hash and have the metadata table store `path -> hash`.
   That's the standard split (cacache, git, OCI layers) and it
   immediately gives us dedup, atomic replace, and torn-write safety.

**Recommendation:**

- Normalize paths once at the runtime boundary
  (`apply_terminal_boundary`, `cache_put_batch`).
- Use typed composite keys `(RecordKind, MountRelPath)` for the
  metadata store, so range scans on a single kind don't have to filter.
- For files, split into:
  - `content` table: key = `blake3(payload)`, value = compressed bytes
    + uncompressed length + refcount.
  - `path_to_blob` table: key = mount-relative path, value = `{ hash,
    size, mtime?, etag? }`.
  - Eviction operates on `content` by LRU; `path_to_blob` follows
    independently.

### Invalidation

The protocol already gives us two granularities: exact path and path
prefix. The persistent layer needs a third in practice: **validators**.

- Exact and prefix invalidation are O(scan-of-matching-keys) under any
  ordered key store. redb and LMDB both do this in O(log n + k);
  hash-only stores like sled (post-bloop) do not. This is a hard
  requirement, not a nice-to-have, given how often providers emit
  prefix invalidations on webhook events.
- **Validators** would let the host re-confirm a record without
  re-fetching the body: provider returns "still good as of ETag X."
  This requires a WIT change (`refresh-if-stale` / conditional fetch)
  that we should not bake in now, but the cache record format should
  reserve a `validator: Option<Validator>` field so we don't need a
  schema bump when the protocol catches up.
- **Negative caching** is already in scope (`LookupPayload::Negative`,
  `DirentsPayload.exhaustive`). Negatives need an upper bound on
  lifetime that's stricter than positives — a deleted file coming back
  is a bigger surprise than a present file going stale. If we
  introduce TTLs (see below), negative records get a separate, shorter
  TTL.

**Open questions:**

- Should `delete_prefix` invalidate validators on surrounding
  parents? E.g. invalidating `/repos/foo/bar/issues/42` arguably
  invalidates the `Dirents` for `/repos/foo/bar/issues`. The current
  code does not, and providers are expected to issue both
  invalidations explicitly. Documenting this is fine; changing it
  introduces a quietly-spreading invalidation we'd regret.
- Should we offer glob/regex invalidation? Probably not. Prefix +
  exact covers webhook-driven invalidation at every provider we have.
  A glob API tempts providers into broad invalidations that defeat
  the cache.

### Expiry

Three viable models:

1. **Capacity only** (today's L2 model, sort of). No TTL. Records live
   until evicted by quota pressure or invalidated. Pros: simplest;
   matches CLAUDE.md's stated rule ("there are no TTLs"). Cons:
   stale-while-disconnected — if a webhook delivery is missed and the
   provider has no other invalidation channel, the cache lies
   forever.
2. **Capacity + per-kind TTL ceiling**. Capacity is the primary
   eviction signal, but each record has a max age (e.g. 24h for
   metadata, 7d for file content, 1h for negatives). Records past
   their TTL are treated as missing. Pros: bounds staleness even on
   webhook gaps. Cons: introduces wall-clock dependency; clock skew
   on cache load matters but doesn't break anything.
3. **Capacity + revalidate-on-use**. Records carry a stamp; old
   stamps trigger a background revalidation while the stale value is
   still served (stale-while-revalidate, RFC 5861). Best UX, most
   complex; needs the conditional-fetch protocol bit we don't have.

**Recommendation:** option 2 for now, with a clean affordance for
upgrading to option 3. Default ceilings:

- `Lookup` positive: 1 day
- `Lookup` negative: 5 minutes
- `Attr`: 1 day
- `Dirents`: 1 hour (cheap to refresh, prone to drift)
- `File` content: 7 days

Make these per-mount overridable in `InstanceConfig.cache`. The
existing CLAUDE.md rule ("no TTLs") was written under the assumption
that webhook invalidation is reliable, which is not generally true
across providers. We should revise that section once this lands.

### Storage quotas per mount

Two-level quota:

- **Per-mount cap** (default e.g. 1 GiB; configurable in mount
  config). Hard cap on summed `len(value)` across the mount's L2.
- **Global cap** (default e.g. 8 GiB). Sum across all mounts.

When a mount is over its cap, its writer triggers eviction in the
background until it's under a low-water mark (90% of cap). When the
global cap is breached, every mount evicts proportionally to its
overage. Implementation:

- Maintain a `ByteAccountant` per mount: a `(bytes_used,
  bytes_high_water, bytes_low_water)` tuple plus a sequence-number
  index for LRU ordering. Updates happen inside the same write
  transaction as the put/delete, so the accountant never disagrees
  with the store.
- Eviction policy per kind: SIEVE for metadata (fits the small,
  pointer-chasey access pattern; cheap O(1) per op; published 2024
  and Twitter-tested), W-TinyLFU for file content (admission control
  on big blobs is what saves you from one large-file scan blowing
  out the cache).
- For content-addressed file blobs: evict by content hash with refcount
  zero first. A blob that's referenced by many `path_to_blob` rows
  shouldn't be evicted because one path got cold.
- **Pinning**: providers may want to mark certain paths as
  "always-cache" (e.g. a manifest file consulted on every list).
  Reserve a `pinned` bit on metadata records; pinned entries skip
  eviction unless the mount is over hard cap. Defer the WIT exposure
  but reserve the bit.

### Durability and atomicity

We want a crash to never produce a half-applied terminal: either the
preload + listing land together, or neither does. redb already gives us
single-DB ACID transactions, which is the level we need. Things to
nail down:

- All cache writes for one terminal go through a single write txn.
  `cache_put_batch` already does this for L2 metadata; if we split
  metadata and content into separate stores, we need a way to keep
  them consistent. Two options:
  - Put both stores in the same redb DB, separate tables. Same txn
    covers both. Simple. Recommended.
  - Use separate engines (e.g. redb for metadata, cacache for
    content). Then writes need a write-ahead log: write content
    blob first, fsync, then write metadata pointing to it. Recovery
    on startup scans `path_to_blob` and orphans content blobs whose
    pointer is missing.
- L0 should not be authoritative on writes. The flow is:
  `provider terminal -> normalize -> L2 write txn (commit) -> L0
  populate`. A panic between L2 commit and L0 populate is benign;
  the next FUSE op rehydrates from L2.
- Invalidation crosses three stores (activity table, L2, kernel). The
  current code doesn't transactionalize this, and we shouldn't try
  to: kernel notification is a notification, not a commit. The
  invariant we want is "L2 is the source of truth." If an
  invalidation lands in L2 but the kernel notify call is dropped
  (the channel is best-effort on Linux), the next path op rebuilds
  the kernel cache from L2.

### Schema migration

`SCHEMA_VERSION: u8 = 2` today silently treats mismatches as misses
(`cache/mod.rs:79`). On every bump, every cache record evaporates.
Acceptable for now; not acceptable as the cache grows to gigabytes.

Two improvements:

- **Versioned tables**, not versioned records. Bump table names
  (`metadata_v3`) when the layout changes. Old tables can be deleted
  by a startup janitor or kept around for a grace period if rollback
  matters.
- **Out-of-band migration**. On open, if the on-disk layout version
  is older than the binary, run a migrator that streams from old
  tables to new and atomically swaps. This matters for record
  formats that can be lossily upgraded; for breaking changes, fall
  back to "drop and rebuild."

A `manifest` table (`schema_version: u32`, `plugin_hash: bytes`,
`written_at: timestamp`, `cache_format_version: u32`) at the top of
each mount's DB makes both possible without any per-record overhead.

### Concurrency

Single-host omnifs is the assumed deployment, but we should be
explicit:

- redb single-writer / multi-reader within a process. Fine.
- redb does not support multi-process access. Two `omnifs mount`
  invocations against the same cache_dir clobber each other. Detect
  this on open via `flock` / `lockf` on a sibling `.lock` file and
  refuse to start cleanly.
- Within the host, the runtime is the only writer; FUSE threads are
  readers. moka L0 already handles concurrent readers.

If we ever want multi-process, that's an FoundationDB / SQLite-WAL
conversation. Not now.

### Encryption at rest

The cache will contain whatever the provider returned: issue bodies,
file contents, account-scoped data, sometimes secrets-adjacent. The
cache_dir is currently a plain directory. Two stances:

1. **No on-disk encryption.** Document that `--cache-dir` should sit
   on a user-private path (XDG `cache_home`, mode 700). Same posture
   as your shell history. Simple, fast.
2. **Optional encryption.** Wrap each value in `chacha20poly1305`
   keyed by an OS-keyring-stored secret. Cost: ~1 μs per record on
   modern hardware, no allocation; keyring integration is the real
   cost. Worth it for shared workstations or when the cache holds
   private repos.

**Recommendation:** ship with (1), make the value codec a trait so (2)
is a 1-day add when a user asks for it.

### Observability

A persistent cache is a lying liability if we can't see into it.
Minimum viable instrumentation:

- Prometheus-style metrics (or just `tracing::info!` counters): hits,
  misses, puts, evictions, invalidations, bytes-in, bytes-out, L0/L2
  ratio, per-mount byte usage.
- A `omnifs cache stat` CLI subcommand that prints per-mount usage,
  hottest paths, and freshness distribution. Cheap; pulls from the
  manifest + a sampling iterator over the metadata table.
- A `omnifs cache invalidate <mount> [--prefix PATH | --path PATH]`
  CLI for manual invalidation when a webhook is missing in action.

## Tech stack survey

Embedded, pure-Rust, single-process, ACID-ish KV stores I've
evaluated. All are ranked against the workload and constraints above.

### redb (incumbent)

- Pure Rust, MVCC, ACID, single-file, copy-on-write B-tree.
- Strengths: simple, no compaction stalls, efficient point + range
  reads, predictable latency, range-deletion is cheap, schema-clean
  via `TableDefinition`. Already in use; we know the failure modes.
- Weaknesses: write amplification on big values (full page rewrite),
  no built-in compression, no built-in eviction. File copying on
  large transactions can be noticeable.
- Verdict: **fine for metadata**, awkward for large file blobs.

### fjall

- Pure Rust LSM (RocksDB-flavored, no C++).
- Strengths: writes-optimized, native key-value separation
  ("blob LSM" mode) where small entries live in the LSM and big
  entries in a side blob log — exactly the metadata/content split we
  want. Built-in compression. Active development.
- Weaknesses: less battle-tested than redb; LSM compaction has
  long-tail latency; on-disk format still evolving.
- Verdict: **best fit if we want a single store**, but a young
  dependency for the foundation of caching.

### cacache

- Content-addressable storage, npm's cache library.
- Strengths: built for exactly the "files at content hash, with
  metadata sidecar" use case. Robust, popular in the JS ecosystem
  (npm install relies on it). Atomic-rename based; crash-safe.
- Weaknesses: filesystem-tax (lots of inodes); poor on Windows; no
  range queries because the metadata is per-key files.
- Verdict: **strong contender for the content store**, paired with
  redb for metadata.

### sled

- Pure Rust embedded DB, log-structured.
- Strengths: easy API, async-friendly.
- Weaknesses: long-running "1.0 soon" status, occasional data-loss
  bugs, the maintainer has signaled a rewrite. Not a load-bearing
  choice today.
- Verdict: **avoid for new code**.

### lmdb / heed

- C library with a thin Rust wrapper (`heed`).
- Strengths: very fast reads (mmap), MVCC, mature.
- Weaknesses: writes are single-threaded, mmap'd files are awkward
  on Windows and across NFS, requires a C dep.
- Verdict: **fine, but not pure Rust**, no advantage over redb for
  this workload.

### rocksdb

- C++ library, the canonical embedded LSM.
- Strengths: every knob in the world, key-value separation
  (BlobDB), industrial uptime.
- Weaknesses: ~25 MB binary cost, opaque to debug, build complexity,
  and we don't need 90% of the features.
- Verdict: **overkill** for a per-host cache.

### sqlite (rusqlite)

- Embedded relational DB.
- Strengths: schema flexibility, ad-hoc queries, mature, WAL gives
  reasonable concurrency.
- Weaknesses: row-oriented overhead for big-blob values; per-call
  syscall overhead higher than mmap'd KV; you'll end up writing
  three tables and indexes manually anyway.
- Verdict: **possible but not better than redb** for our shape.

### Filesystem with xattrs

- Just put files in a dir tree, store metadata in xattrs.
- Strengths: trivial, the OS does paging.
- Weaknesses: xattrs are size-capped (~64 KiB on ext4, ~128 KiB on
  apfs), portability is grim, atomic updates require fsync of the
  whole dir, no range queries.
- Verdict: **no**.

## Recommended architecture

Two-store, single-DB-per-mount, over redb, with a content-addressed
blob table. We stay on redb for the foundation (proven, pure Rust,
already in tree) and split storage into three tables that play to
redb's strengths and avoid its weaknesses on big values.

```
{cache_dir}/providers/{mount}/{plugin_hash}/
  ├─ browse.redb              # all tables below in one DB
  └─ .lock                    # process-exclusive lockfile
```

Tables inside `browse.redb`:

| Table          | Key                              | Value                                      | Notes                                  |
|----------------|----------------------------------|--------------------------------------------|----------------------------------------|
| `manifest`    | `()` (singleton)                  | `Manifest` (versions, plugin hash, mount)  | Read on open, validated.               |
| `metadata`    | `(RecordKind, NormalizedPath)`    | `MetaRecord` (postcard)                    | Lookup, Attr, Dirents.                 |
| `path_to_blob`| `NormalizedPath`                  | `{ hash: [u8; 32], size, validator? }`     | Small. Indirection layer.              |
| `blob`        | `[u8; 32]` (blake3)               | `{ refcount, len, codec, bytes }`          | All file content. Dedup'd. Compressed. |
| `lru_meta`    | `(seq: u64, RecordKind, path)`    | `()`                                       | LRU index for metadata eviction.       |
| `lru_blob`    | `(seq: u64, hash)`                | `()`                                       | LRU index for blob eviction.           |

`MetaRecord` shape:

```rust
struct MetaRecord {
    schema_version: u32,
    written_at: u64,           // unix seconds; for TTL ceiling
    expires_at: Option<u64>,   // pre-computed, kind-dependent
    validator: Option<Validator>, // ETag/version; reserved
    flags: MetaFlags,          // pinned, exhaustive, negative, etc.
    payload: Vec<u8>,          // postcard-encoded payload-per-kind
}
```

The crucial decisions:

- **One redb DB per mount; tables, not databases, for separation.**
  Single write txn covers metadata + path_to_blob + blob + lru index
  updates atomically. No WAL needed.
- **Content addressing.** `path_to_blob` holds the small indirection;
  `blob` holds bytes keyed by hash with a refcount. New writes
  increment refcount; deletions decrement and GC at zero. Two paths
  to the same content cost one blob.
- **Compression on the blob table only.** Gate on `len >= 4 KiB` to
  avoid wasting CPU on already-tiny payloads. zstd level 3 by
  default.
- **LRU as side indexes.** Each metadata or blob mutation also
  inserts into `lru_meta` or `lru_blob` keyed by a monotonic
  sequence. Eviction walks ascending `lru_*` until enough bytes are
  freed. The seq counter lives in `manifest` and is bumped per
  txn-batch.
- **TTL ceilings on metadata via `expires_at`.** Reads check the
  field; expired rows are treated as misses. A background sweeper
  removes them at low priority — fine to be lazy.
- **Quota enforcement on write.** Every txn computes net byte delta;
  if the post-write `bytes_used` would exceed the high-water mark,
  the same txn evicts from `lru_*` until under low-water. This keeps
  consistency without needing a separate eviction process.
- **Path normalization in one place.** `NormalizedPath` is a newtype
  around an `Arc<str>` that runs `normalize` (collapse `//`, strip
  trailing `/`, reject `..`) once at construction. Used everywhere
  internally.
- **Plugin-hash-scoped subdir.** A plugin upgrade simply opens a new
  subdir; a startup janitor removes subdirs older than the current
  hash that haven't been used in N days. No in-place migration.

### Wire to the existing runtime

The split is mechanical:

- `cache_put_batch(records)` becomes:
  - For metadata records: insert into `metadata`, `lru_meta`.
  - For File records: hash, lookup `blob`, insert if new, increment
    refcount, insert path mapping in `path_to_blob`, insert into
    `lru_blob` if blob is new.
- `cache_delete_path(path)` decrements blob refcount before removing
  `path_to_blob` mapping; metadata rows are deleted directly.
- `cache_delete_prefix(prefix)` does the same in batches under a
  range scan.
- L0 stays inode-keyed in memory; the read path does L0, then L2,
  with the L2 read transparently following `path_to_blob -> blob`.
  L0 entries cap at the existing 256 KiB threshold (so big blobs go
  L2-only, which the current code already enforces).

### Wire to the FUSE layer

No changes required. The FUSE layer reads `LookupPayload`,
`DirentsPayload`, `AttrPayload`, and file bytes. The shape of those
payloads is preserved; only the storage substrate changes.

### Wire to the protocol

No WIT changes required for this pass. The shape is forward-compatible
with future additions:

- Add a `validator: option<validator>` field to terminals when the
  protocol grows conditional fetch — slot already reserved in
  `MetaRecord`.
- Add a `pinned: bool` field to terminals when the protocol grows
  pinning — slot already reserved.

## Prototype plan

Three small, executable steps. Each is mergeable and improves the cache
on its own.

### Prototype 1: schema-versioned manifest + plugin-hash subdir

Smallest possible. Adds `manifest` table and a plugin-hash-scoped
subdirectory. Lets us bump the cache layout without losing
already-cached data and gives us a single place to read mount
metadata.

- Add `Manifest { schema_version: u32, plugin_hash: [u8; 32], created_at:
  u64, mount: String }`.
- Compute plugin hash at runtime construction (we already load the
  WASM bytes; sha256 them).
- Path becomes
  `{cache_dir}/providers/{mount}/{plugin_hash}/browse.redb`.
- On open: read manifest, fail loudly if `schema_version` mismatches
  the binary's expectation.
- Startup janitor: walk `{cache_dir}/providers/{mount}`, delete
  subdirs whose hash isn't the current one and whose mtime is >7
  days old.

Risk: low. Touches `BrowseCacheL2::open`, runtime construction,
nothing else. ~150 lines.

### Prototype 2: byte accountant + per-mount quota with SIEVE eviction

Adds `lru_meta` and the byte accountant. Makes the cache
self-bounding.

- Add `bytes_used: AtomicU64` per mount, persisted in `manifest`,
  refreshed at open by summing table sizes (one-time scan).
- Add `lru_meta` side index. Sequence counter in `manifest`.
- On every put/delete, update accountant in same txn.
- On put that overflows high-water: evict `lru_meta` ascending until
  under low-water. Use SIEVE-style hand to skip recently-touched
  rows (a single bit per row, set on read, cleared by the eviction
  hand).
- Default quota: 1 GiB per mount, 8 GiB global. Configurable in
  `InstanceConfig.cache.max_bytes`.
- Metric counters via `tracing::info!`: hits, misses, evictions,
  bytes_used.

Risk: medium. Touches L2 internals; needs care around txn lifetime
when an eviction is triggered inside a put. ~400 lines.

### Prototype 3: content-addressed blob store with refcount

Splits the `content` and `bulk` tables into `path_to_blob` + `blob`.
Gives us dedup, atomic large-value writes, and natural blob-level LRU.

- Add `path_to_blob` and `blob` tables.
- Migration: on first open with the new schema, stream existing
  `content` and `bulk` rows into `blob` (computing blake3 on
  insert), populate `path_to_blob`, drop the old tables. One-shot;
  acceptable since the data is recoverable from providers.
- File reads follow indirection.
- File writes hash, dedup, refcount.
- Compression: zstd level 3 on values >= 4 KiB.
- Plug into Prototype 2's accountant: `lru_blob` and the same evict
  loop, scaled to evict from blobs first when the mount is over
  quota and content dominates the footprint.

Risk: medium-high. Migration needs care. The compression decision
should be a 5-line trait so we can revisit zstd vs lz4 vs none with
real numbers. ~700 lines.

### Optional follow-ups

- **TTL ceiling sweeper.** Background task that scans `metadata` for
  expired rows. Cheap, lazy. Adds ~50 lines.
- **`omnifs cache stat` and `omnifs cache invalidate` subcommands.**
  Pure CLI plumbing, ~150 lines.
- **Encryption codec.** Trait + `chacha20poly1305` impl behind a
  feature flag, key from OS keyring. ~200 lines.
- **L0 global cap.** Replace per-mount moka with a shared moka
  across mounts, weighted by record kind. ~50 lines.

## Risks and open questions

- **Compression CPU vs cache hit cost.** zstd level 3 is ~500 MB/s
  per core on modern hardware; for a cache that mostly serves hits,
  this is invisible. For a cold start that backfills from providers,
  the cost is real. Benchmark at prototype 3.
- **TTL ceilings vs CLAUDE.md.** CLAUDE.md says "no TTLs"; this
  proposal adds them as ceilings (not per-record TTLs in the old
  sense). Update CLAUDE.md if/when prototype 2 lands, or skip TTLs
  entirely and rely on quota + provider invalidation. Decision
  needed.
- **Plugin-hash churn.** If providers are rebuilt on every dev
  iteration, the plugin-hash subdir thrashes. Either (a) base the
  hash on the WIT world rather than the bytes, (b) gate the cache
  wipe behind a `--strict-provider` flag, or (c) have the janitor
  keep the previous N hashes warm. Decision needed.
- **redb durability fsync cost.** `commit()` fsyncs by default. For
  a burst of preloads, that's one fsync per terminal. If this shows
  up in profiles, batch terminals at the runtime layer rather than
  weakening durability per-txn.
- **Multi-process safety.** `flock` works on Linux/macOS. On
  Windows, `LockFileEx` is the equivalent. Worth a 30-line
  cross-platform wrapper now, before someone tries to run two
  hosts and discovers the silent corruption mode.
- **Negative-cache poisoning.** If a provider transiently returns
  "not found" for a path that's actually a permission error, we
  cache the negative. Short TTL on negatives is the mitigation;
  long-term we want providers to distinguish "absent" from
  "forbidden" in the protocol. Out of scope here.

## Summary

Stay on redb. Add a manifest, plugin-hash scoping, a byte accountant,
SIEVE-style eviction, content-addressed blobs with refcounts, and
optional zstd compression. Don't change WIT. Land in three prototypes
of decreasing safety and increasing payoff. The result is a persistent
cache that behaves well under restart, doesn't grow unbounded, dedups
file content, survives plugin upgrades, and leaves an obvious door
open for validators and pinning when the protocol catches up.
