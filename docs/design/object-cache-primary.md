# Object cache as the durable primary

Status: implemented
Related: `architecture.md` §3-§5 (the cache invariants this doc details), `file-attributes.md`.

## Decision

The **object cache** (canonical upstream bytes) is the durable, primary host cache. The **view cache** (rendered bytes shell tools read) is derived and recomputable from it without an upstream refetch. This inverts the older model, where the view tier was the durable store and the canonical store was a volatile in-memory LRU.

Three host caches, all byte-level, named by role:

| Cache | Role | Durable | Fill |
|---|---|---|---|
| **object** | canonical upstream bytes, keyed by logical object id | yes (primary) | object-shaped providers only |
| **view** | rendered representations / fields / dirents | no (derived, rebuilt on startup) | any provider |
| **blob** | large binary served by handle (+ archive trees) | yes, on disk | callout-driven |

A structural provider never emits `canonical-store`, so its object cache stays empty and **self-selects out**. There is no manifest flag and no host-side "is object-shaped" branch.

## Why the inversion

The canonical object is the expensive thing (an upstream fetch); every rendered representation is cheap and local (parse the canonical, render). So the canonical must outlive the renders, not the other way around. Once the object cache is durable:

- A view miss on `item.json` after `item.md` re-renders from the cached object with **no upstream call** — across process restarts.
- Identity representations are not double-stored. The host read path already refuses to copy a `byte-source::canonical` result into the view cache; making the object cache durable is what closes the loop.

## The read path (design from the call site backward)

```rust
// omnifs-host: reads like the domain operation; the Store knows its mount.
async fn read_file(&self, path: &Path, ct: ContentType) -> Result<ReadFileResult> {
    if let Some(hit) = self.store.view_get(path, RecordKind::File) {
        return Ok(hit.into());                          // view hit: serve rendered bytes
    }
    let op_gen = self.store.current_generation();       // capture BEFORE the push, so push + write share one fence point
    let pushed = self.store.cached_canonical_for(path);  // exact map -> { id, bytes, validator }, or None
    let ret = self.runtime.read_file(path, ct, pushed).await?;
    // Fenced: a canonical-store overwrite AND a view write derived from a pushed canonical are
    // dropped if path's id was invalidated after op_gen (Codex #1).
    Materializer::new(&self.store).apply(&ret.effects, op_gen, now_millis);
    Ok(ret.result)
}
```

- `pushed = Some` means the SDK renders from the pushed canonical, no fetch.
- `pushed = None` means the SDK fetches (object-shaped) or returns structural bytes; the resulting `canonical-store` effect both stores the object and teaches the path-to-id map.

There is one read path and no dispatch fork. The host never derives object identity: `cached_canonical_for` is an **exact map lookup**, learned from effects, not a prefix probe.

## Types and ownership

`omnifs-cache` owns both object and view storage. It is a wire-agnostic
**byte-storage** crate: it stores and retrieves records, it does not interpret
the provider protocol. Path types live in `omnifs-core`; provider WIT conversion
and effect materialization live in `omnifs-host`.

```
omnifs-cache/
  object.rs   object::Cache   durable; id → Canonical; paths index
  view.rs     view::Cache     moka `mem` tier + fjall `disk` tier
  lib.rs      Caches, Store    per-mount facade; RecordKind + batch/record types; storage primitives
```

`FileAttrs`, `FileSize`, `ByteSource`, and `Stability` are the attribute DTOs
stored on records; they live in `omnifs-core/src/view.rs`, not in
`omnifs-cache`.

**Materialization is the host's job, not the cache's.** `Effects` is the provider's wire terminal and projection is a provider-authoring concept; neither belongs in a byte store. The host already owns the wire bridge (`crates/omnifs-host/src/wit_protocol.rs`, `namespace.rs`), so it also owns the materializer (`crates/omnifs-host/src/materialize.rs`) that decomposes a provider return into cache primitive calls, and the read-path content-type derivation. The cache exposes only `put_canonical` / `put_canonical_batch` / `cached_canonical_for` / `delete_listing_path[_prefix]` / `delete_object`.

```rust
// omnifs-host: the materializer bundles the store handle; apply takes the per-return wire effects.
struct Materializer<'a> { store: &'a Store }

impl Materializer<'_> {
    // Returns (invalidated_prefixes, invalidated_paths); FUSE adapters turn those into
    // kernel cache-invalidation notifications.
    fn apply(&self, effects: &wit::Effects, op_gen: u64, now_millis: u64) -> (Vec<String>, Vec<String>) {
        // Canonical-store entries that pass conflict detection are collected and committed in one
        // fjall write batch. put_canonical evicts the id's prior derived view leaves before
        // storing new ones, so an evict-then-refetch can't leave a mixed-version view (Codex #6).
        // Fenced by op_gen.
        let batch = collect_canonical(&effects.canonical);
        self.store.put_canonical_batch(batch, op_gen);
        // fs writes land as view records via cache_put_batch (dirents) and cache_view_leaf
        // (file content, carrying a Stability-derived freshness deadline); the dirent
        // read-modify-write stays transactional inside the cache, not a host merge (Codex #8).
        write_fs_effects(self.store, &effects.fs, op_gen, now_millis);
        // invalidations cascade through delete_object / delete_listing_path / delete_listing_prefix.
        apply_invalidations(self.store, &effects.invalidations)
    }
}
```

### Domain values

The shipped cache keys on primitive types rather than dedicated newtypes: the
logical object id is opaque bytes (`Vec<u8>` / `ObjectId`), the mount is a
`String` (`Caches::mount` takes `impl Into<String>`), the fence generation is a
`u64` backed by an `AtomicU64`, and the validator is `Option<String>`
(non-empty, opaque, byte-compared; `op_validate` rejects empty or oversized
tokens in release). View-leaf paths are absolute strings indexed in the path
map.

`Canonical` is the stored object; `content_type` is gone (it was write-only state — the push carries only bytes + validator, and the served content type comes from the path suffix or the per-entry view record):

```rust
pub struct Canonical { pub bytes: Vec<u8>, pub validator: Option<String> }
```

### Global caches, per-mount facade

The caches are process-global handles opened once; `Store` is the cheap per-mount view that owns the **single mount-scoping policy site**. Recurring context (mount + handles) is bundled here so no method threads `(mount, ...)` repeatedly.

```rust
pub struct Caches { object: object::Cache, view: view::Cache }   // durable object DB; disposable view DB (cleared on startup)

impl Caches {
    pub fn open(dir: &Path) -> anyhow::Result<Arc<Self>>;            // open durable object DB; clear + reopen view DB (always cold)
    pub fn mount(self: &Arc<Self>, mount: impl Into<String>) -> Store; // per-mount handle
}

pub struct Store { caches: Arc<Caches>, mount: String }

impl Store {
    // reads
    pub fn view_get(&self, path: &ProtocolPath, kind: RecordKind) -> Option<Record>;
    // exact id map → (logical id, bytes, validator); the SDK self-checks the id (Codex #2).
    pub fn cached_canonical_for(&self, path: &ProtocolPath) -> Option<(Vec<u8>, Vec<u8>, Option<String>)>;

    // storage primitives (the host materializer calls these; the cache does not parse effects)
    pub fn put_canonical(&self, id: &[u8], bytes: Vec<u8>, validator: Option<String>, view_leaves: &[String], op_gen: u64) -> bool;
    pub fn put_canonical_batch(&self, entries: Vec<CanonicalBatchEntry>, op_gen: u64);
    pub fn cache_put_batch(&self, records: &[BatchRecord]);          // transactional dirent merge (Codex #8)
    pub fn cache_view_leaf(&self, path: &ProtocolPath, ..., expires_at: Option<u64>, op_gen: u64); // fenced (Codex #1)
    pub fn delete_object(&self, id: &[u8]);         // drops the object + its indexed leaves; a leaf invalidation resolves to its id first (Codex #3)
    pub fn delete_listing_path(&self, path: &ProtocolPath);
    pub fn delete_listing_prefix(&self, prefix: &ProtocolPath);
    pub fn current_generation(&self) -> u64;        // per-mount clock (Codex #7)

    fn scoped(&self, path: &Path) -> Path;          // "/{mount}{path}" — view-tier mount scoping
}
```

The push carries the logical id (Codex #2: the SDK self-checks it) and the
`op_gen` captured before the push (Codex #1: fence the render). The object tier
is structurally isolated per mount (its own keyspaces), so its keys are raw; the
shared view tier is isolated by the `/{mount}` path prefix `Store::scoped`
applies. The fence clock and tombstones are **per-mount** (Codex #7): a noisy
mount can no longer GC another mount's live tombstone.

### object::Cache

```rust
pub struct Cache {
    db: fjall::Database,   // per-mount keyspaces: objects.{mount} id → { canonical, leaves };  view.{mount} leaf → id
}

impl Cache {
    pub fn store(&self, scoped_id: &[u8], c: Canonical, new_leaves: &[String], view_evict: impl FnMut(&str)) -> bool;
    pub fn get(&self, scoped_id: &[u8]) -> Option<StoredObject>;
    pub fn id_of(&self, scoped_path: &[u8]) -> Option<Vec<u8>>; // exact lookup; replaces the prefix probe
    pub fn leaves_of(&self, scoped_id: &[u8]) -> Vec<String>;   // reverse set, for exact eviction (Codex #4)
    pub fn evict_object(&self, scoped_id: &[u8], view_evict: impl FnMut(&str)); // drop object + every indexed leaf, exactly
}
```

The per-mount fence (generation counter plus tombstone map) lives on `Store`, not on `Cache`; the cache is a mount-agnostic byte store. `Store::put_canonical` admits a `store` only if no tombstone for the id is newer than `op_gen`. The store then **evicts the id's prior derived view leaves** (so an evict-then-refetch can't strand a mixed version, Codex #6), persists `objects[id] = { canonical, leaves }`, and inserts `paths[leaf] = id` for each leaf. Eviction reads `leaves_of(id)` and deletes those **exact** full paths, never a textual or segment prefix, which is unsafe for File-shape leaves (Codex #4).

## Invariants

1. **Exact leaf eviction.** The path index is keyed by full leaf path; eviction of an object deletes its **exact** leaves via the object record's reverse set, never a prefix scan. Textual/segment prefix deletion is unsafe: a File-shape path `/issues/45` is a textual prefix of both `/issues/45.md` (wanted) and `/issues/45a.md` (not), and `/issues/45.md` is not a segment child of `/issues/45` (Codex #4).
2. **Id coherence on the push.** `canonical-input` carries the pushed logical id; the SDK renders only if it matches the route-derived id, else treats the canonical as absent and fetches (Codex #2). A wrong or stale map entry degrades to a refetch, never silently-wrong bytes.
3. **One canonical per logical id**, overwritten when the validator advances; the overwrite evicts the object's prior derived views first (Codex #6). No version history, no content-addressing (a 304 emits no write; aliases collapse to one id). Keyed by logical id plus mount, not hashed content.
4. **The fence covers every write derived from a pushed canonical**, not just `canonical-store`. A rendered view result with no `canonical-store` is fenced against the `op_gen` captured before the push (Codex #1). The fence clock + tombstones are **per-mount** and runtime-only (Codex #7); nothing survives a restart, so they are not persisted.
5. **Leaf invalidation cascades to the object.** Invalidating a representation path (which `architecture.md` §5 does on version advance) consults the path index and, if the path is a known leaf, evicts the whole object + its leaves, never just the one view entry (Codex #3).
6. **Object cache is opt-in.** Structural providers emit no `canonical-store`; the cache self-selects. No host shape declaration.
7. **No double storage.** Identity representations (`byte-source::canonical`) live only in the object cache; the view never copies them.
8. **View is disposable.** A separate fjall database, cleared and reopened on every startup — no crash-detection, no sentinel. It never survives a restart to disagree with the durable object (Codex #5); object and view need no cross-store atomicity.
9. **Schema decoupled from wire.** `omnifs-cache` has no `omnifs-wit` dependency; the durable serde schema is versioned independently; the host owns the wire bridge.
10. **Release-validated leaves.** `op_validate` (not a `debug_assert`) rejects empty logical ids, empty or invalid view leaves, invalidation ids, and oversized validator tokens (Codex #10).
11. **Mount isolation, no in-key separator.** The object tier gets a keyspace per mount (raw keys); the view tier prefixes keys with `/{mount}`. Cross-mount reads are unrepresentable.

## Wire delta (`architecture.md` §4 ↔ shipped WIT)

The shipped WIT stores object identity explicitly. `canonical-store` carries a
provider logical id, validator, bytes, and full absolute view leaves. The dead
`tree-handoffs` effect block is removed; handoff already rides the
`op-result::subtree` arm plus `TreeRefs`.

```wit
record logical-id {
    kind: string,
    captures: list<id-capture>,
}

record canonical-store {
    id: logical-id,
    validator: option<version-token>,
    bytes: list<u8>,
    view-leaves: list<string>,
}

record fs-write {
    id: option<logical-id>,
    path: string,
    kind: fs-kind,
}

record effects {
    canonical: list<canonical-store>,
    fs: list<fs-write>,
    invalidations: list<invalidation>,
}

record canonical-input {
    id: logical-id,
    validator: option<version-token>,
    bytes: list<u8>,
}
```

The host stores `view-leaves` on the object record as the reverse set, so
eviction has the exact full leaf list without a prefix scan. The SDK expands
finite facet aliases before emitting the effect, so paths such as
`/issues/open/42/title` and `/issues/all/42/title` can point at the same
logical id.

## Storage

The object cache (precious, low write rate) is **durable and global**; the view cache (high write rate, disposable) is **non-durable** — a separate fjall database cleared and reopened on every startup. This split is what makes the contention and atomicity findings tractable, with no bespoke crash-detection machinery. Both tiers are fjall (LSM-tree) databases.

```
<cache-dir>/object/                  durable, global, survives restart (fjall database)
    objects.{mount} : "{anchor}"      -> { canonical, leaves }   // leaves = reverse set for exact eviction
    view.{mount}    : "{leaf-path}"   -> anchor                  // forward map for the read-path push
<cache-dir>/view/                    non-durable: cleared + reopened on every startup (fjall database)
    metadata/content/bulk : "{kind}:/{mount}{path}"[\x1f<hex-aux>]
```

- **Object DB: per-mount keyspaces, raw keys.** Mount isolation is structural (a keyspace per mount), so the key carries no mount prefix and there is no in-key separator. Object writes happen only on a cold fetch, so contention is negligible (Codex #9).
- **View DB: a separate fjall database, cleared on startup.** No sentinel, no clean-vs-crash distinction — the host deletes the `view/` directory at startup and reopens it empty, so the view is always cold after any restart and can never survive to disagree with the durable object (Codex #5). The view recomputes from the object with no upstream refetch. Because the view is disposable, object and view need no cross-store atomicity. The view keyspaces are shared across mounts; isolation comes from the `/{mount}` path prefix on each key.

Budgets: object DB global LRU; view LRU; in-memory `mem` tier (moka) over each. Per-mount fairness for the object DB is deferred until a noisy mount is shown to starve a quiet one.

"No TTL" is precise only for the object cache: canonical bytes carry no time deadline and leave by capacity eviction or explicit invalidation. View leaves do carry a `Stability`-derived freshness deadline: `freshness_expiry` sets a `Dynamic` leaf to `now + 3000ms` and a `Live` leaf to `now` (immediate), and `view_get` drops a leaf past its deadline (`crates/omnifs-host/src/clock.rs`, `materialize.rs`; `crates/omnifs-cache/src/lib.rs`). The negative cache likewise carries TTLs. Providers still add no TTLs or LRUs of their own.

## Tradeoffs accepted

- **Object writes commit per cold fetch.** fjall has no global writer lock; object writes (batched objects + view-index commits) happen only on a cold fetch, so they barely contend.
- **View recompute after any restart.** Startup clears the view DB, so the first read of each path after a restart re-renders from the durable object (local, no upstream). Cheaper than the machinery to keep a warm view across clean shutdowns.
- **Large ranged `.raw`.** A `Deferred { Ranged }` object `.raw` still has no `read-file` byte source; remains deferred per `architecture.md` §8.

## Adversarial review (Codex gpt-5.5, accepted findings)

The design was hardened against an adversarial review. Folded in above:

- **#1 (Critical)** push captured before `op_gen` and the rendered result re-cached after a mid-op invalidation → the `op_gen` captured before the push fences the view write.
- **#2 (Critical)** wrong/stale map silently renders -> `canonical-input` carries the logical id; SDK self-checks before rendering.
- **#3 (Critical)** invalidating a representation path left the object alive -> leaf invalidation cascades to the logical id via the path index.
- **#4 (High)** File-shape breaks prefix deletion -> exact-leaf eviction from the object's reverse leaf set; the "textual-containment means free range-delete" claim is withdrawn.
- **#5 (High)** cross-DB invalidation isn't crash-atomic → the view is a separate fjall database cleared on every startup, so no view survives a restart to disagree with the object.
- **#6 (High)** object eviction + refetch -> mixed versions -> a `canonical-store` overwrite evicts the object's prior derived views first.
- **#7 (High)** global generation + tombstone GC unsafe across mounts -> the fence is per-mount.
- **#8 (Medium)** concurrent dirent merge lost-update -> the dirent merge runs through fjall's optimistic `update_fetch`, which reruns on a write-write conflict and always merges onto the latest value, so plain writes never lose a concurrent merge.
- **#9 (Medium)** global write-lock contention -> fjall has no global writer lock. Object writes are cold-fetch-only, so they barely contend; the shared view keyspaces take lock-free plain writes plus the optimistic merge above.
- **#10 (Medium)** `leaves` only `debug_assert`ed → release validation in `op_validate`.

Confirmed fine: structural files under an object dir; dropping `content-type`.
