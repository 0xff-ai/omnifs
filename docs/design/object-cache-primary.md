# Object cache as the durable primary

Status: implemented
Supersedes: `docs/design/cache-architecture.md`. Revises ADR-0001 §4 (caches), §5–6 (read/write path), §9 (WIT effects).

## Decision

The **object cache** (canonical upstream bytes) is the durable, primary host cache. The **view cache** (rendered bytes shell tools read) is derived and recomputable from it without an upstream refetch. This inverts today's code, where the view tier is durable (`browse.redb`) and the canonical store is a volatile in-memory LRU.

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
- Identity representations are not double-stored. `read.rs` already refuses to copy a `byte-source::canonical` result into the view cache; making the object cache durable is what closes the loop.

## The read path (design from the call site backward)

```rust
// omnifs-host: reads like the domain operation; the Store knows its mount.
async fn read_file(&self, path: &Path, ct: ContentType) -> Result<ReadFileResult> {
    if let Some(hit) = self.store.view_get(path, RecordKind::File) {
        return Ok(hit.into());                          // view hit: serve rendered bytes
    }
    let op_gen = self.store.current_gen();              // capture BEFORE the push, so push + write share one fence point
    let pushed = self.store.canonical_for(path);        // exact map -> { id, bytes, validator }, or None
    let ret = self.runtime.read_file(path, ct, pushed).await?;
    // Fenced: a canonical-store overwrite AND a view write derived from a pushed canonical are
    // dropped if path's anchor was invalidated after op_gen (Codex #1). apply() needs `path` to
    // fence the rendered-result view write, not just the effect writes.
    Materializer::new(&self.store).apply(&ret, op_gen, path);
    Ok(ret.result)
}
```

- `pushed = Some` means the SDK renders from the pushed canonical, no fetch.
- `pushed = None` means the SDK fetches (object-shaped) or returns structural bytes; the resulting `canonical-store` effect both stores the object and teaches the path-to-id map.

There is one read path and no dispatch fork. The host never derives object identity: `canonical_for` is an **exact map lookup**, learned from effects, not a prefix probe.

## Types and ownership

`omnifs-cache` owns both object and view storage. It is a wire-agnostic
**byte-storage** crate: it stores and retrieves records, it does not interpret
the provider protocol. Path types live in `omnifs-core`; provider WIT conversion
and effect materialization live in `omnifs-host`.

```
omnifs-cache/
  object.rs   object::Cache   durable; anchor → Canonical; paths index; fence
  view.rs     view::Cache     moka `mem` tier + fjall `disk` tier
  store.rs    Store           per-mount facade: storage primitives only
  record.rs   RecordKind, Record, BatchRecord, Dirents/FilePayload
  attrs.rs    FileAttrs, FileSize, ByteSource, Stability  (attribute DTOs stored on records)
  key.rs      Key, Anchor, VersionToken, Generation, MountName
```

**Materialization is the host's job, not the cache's.** `Effects` is the provider's wire terminal and projection is a provider-authoring concept; neither belongs in a byte store. The host already owns the wire bridge (`host/src/cache`, `host/src/view.rs`), so it also owns the materializer that decomposes a provider return into cache primitive calls, and `content_type_for` (read-path content-type derivation). The cache exposes only `object_store` / `view_put_batch` / `canonical_for` / `invalidate[_prefix]`.

```rust
// omnifs-host: the materializer bundles the store handle; methods take the per-return wire effects.
struct Materializer<'a> { store: &'a Store }

impl Materializer<'_> {
    fn apply(&self, ret: &wit::ProviderReturn, gen: Generation, read_path: &Path) -> Invalidations {
        for c in &ret.effects.canonical {
            // object_store evicts the anchor's prior derived view leaves before storing new ones,
            // so an evict-then-refetch can't leave a mixed-version view (Codex #6). Fenced by gen.
            self.store.object_store(&c.anchor, &Canonical::from(c), &c.leaves, gen);
        }
        for fs in &ret.effects.fs {
            // dirent RMW is transactional inside the cache, not a host read-merge-write (Codex #8).
            self.store.project(fs, gen);
        }
        // A rendered-from-pushed-canonical result (no canonical-store) is still a view write:
        // fence it against read_path's anchor so a mid-op invalidation isn't re-cached (Codex #1).
        self.store.cache_read_result(read_path, &ret.result, gen);
        ret.effects.invalidations.iter().map(|inv| self.store.invalidate_one(inv)).collect()
    }
}
```

### Domain newtypes (invariants at construction)

```rust
pub struct Anchor(Path);                 // an object's key: a validated object-anchor path
pub struct VersionToken(String);         // non-empty, <= 256 bytes; opaque; byte-compared
pub struct Generation(u64);              // fence clock
pub struct MountName(String);            // key-scope prefix; validated, no separator chars
```

`Canonical` is the stored object; `content_type` is gone (it was write-only state — the push carries only bytes + version, and the served content type comes from the path suffix or the per-entry view record):

```rust
pub struct Canonical { pub bytes: Vec<u8>, pub version: Option<VersionToken> }
```

### Global caches, per-mount facade

The caches are process-global handles opened once; `Store` is the cheap per-mount view that owns the **single mount-scoping policy site**. Recurring context (mount + handles) is bundled here so no method threads `(mount, ...)` repeatedly.

```rust
pub struct Caches { object: object::Cache, view: view::Cache }   // object keyspace (durable); view keyspace (deleted on startup)

impl Caches {
    pub fn open(dir: &Path) -> anyhow::Result<Arc<Self>>;        // open durable object DB; delete + recreate view keyspace (always cold)
    pub fn mount(self: &Arc<Self>, mount: MountName) -> Store;   // per-mount handle
}

pub struct Store { caches: Arc<Caches>, mount: MountName }

// The push carries the anchor (Codex #2: SDK self-checks) and hit_gen (Codex #1: fence the render).
pub struct Pushed { pub anchor: Anchor, pub bytes: Vec<u8>, pub version: Option<VersionToken>, pub hit_gen: Generation }

impl Store {
    // reads
    pub fn view_get(&self, path: &Path, kind: RecordKind) -> Option<Record>;
    pub fn canonical_for(&self, path: &Path) -> Option<Pushed>;          // exact anchor map → push

    // storage primitives (the host materializer calls these; the cache does not parse effects)
    pub fn object_store(&self, anchor: &Anchor, c: &Canonical, leaves: &[Leaf], op_gen: Generation) -> bool;
    pub fn project(&self, write: &FsWriteView, op_gen: Generation);      // transactional dirent merge (Codex #8)
    pub fn cache_read_result(&self, path: &Path, result: &ReadResultView, op_gen: Generation); // fenced (Codex #1)
    pub fn invalidate(&self, path: &Path);          // cascades a leaf to its anchor (Codex #3)
    pub fn invalidate_prefix(&self, prefix: &Path);
    pub fn current_gen(&self) -> Generation;        // per-mount clock (Codex #7)

    fn scoped(&self, key: &str) -> String;          // "{mount}\x1f{key}" — the only place mount enters a key
}
```

The caches operate on already-scoped keys; they are mount-agnostic byte stores. Mount isolation lives in `Store::scoped` alone. The fence clock and tombstones are **per-mount** (Codex #7): a noisy mount can no longer GC another mount's live tombstone.

### object::Cache

```rust
pub struct Cache {
    db: fjall::Database,    // objects: scoped-anchor → { canonical, leaves };  paths: scoped-leaf → scoped-anchor
    fence: Fence,         // runtime-only, per-mount: gen + tombstones; reset on restart
}

impl Cache {
    pub fn store(&self, anchor: &str, c: &Canonical, leaves: &[Leaf], op_gen: Generation) -> bool;
    pub fn get(&self, anchor: &str) -> Option<Canonical>;
    pub fn anchor_of(&self, path: &str) -> Option<String>;   // exact lookup; replaces the prefix probe
    pub fn leaves_of(&self, anchor: &str) -> Vec<String>;    // reverse set, for exact eviction (Codex #4)
    pub fn evict_anchor(&self, anchor: &str);                // drop object + every indexed leaf, exactly
    pub fn current_gen(&self) -> Generation;
}
```

`store` is admitted only if no tombstone for `anchor` is newer than `op_gen` (the fence). It then **evicts the anchor's prior derived view leaves** (so an evict-then-refetch can't strand a mixed version, Codex #6), persists `objects[anchor] = { c, leaves }`, and inserts `paths[anchor + leaf] = anchor` for each leaf. Eviction reads `leaves_of(anchor)` and deletes those **exact** full paths — never a textual or segment prefix, which is unsafe for File-shape leaves (Codex #4).

## Invariants

1. **Exact leaf eviction.** The path index is keyed by full leaf path; eviction of an object deletes its **exact** leaves via the object record's reverse set, never a prefix scan. Textual/segment prefix deletion is unsafe: a File-shape path `/issues/45` is a textual prefix of both `/issues/45.md` (wanted) and `/issues/45a.md` (not), and `/issues/45.md` is not a segment child of `/issues/45` (Codex #4).
2. **Id coherence on the push.** `canonical-input` carries the pushed logical id; the SDK renders only if it matches the route-derived id, else treats the canonical as absent and fetches (Codex #2). A wrong or stale map entry degrades to a refetch, never silently-wrong bytes.
3. **One canonical per logical id**, overwritten when the validator advances; the overwrite evicts the object's prior derived views first (Codex #6). No version history, no content-addressing (a 304 emits no write; aliases collapse to one id). Keyed by logical id plus mount, not hashed content.
4. **The fence covers every write derived from a pushed canonical**, not just `canonical-store`. A rendered view result with no `canonical-store` is fenced against its read path's object id at `hit_gen` (Codex #1). The fence clock + tombstones are **per-mount** and runtime-only (Codex #7); nothing survives a restart, so they are not persisted.
5. **Leaf invalidation cascades to the object.** Invalidating a representation path (which ADR-0001 §6 does on version advance) consults the path index and, if the path is a known leaf, evicts the whole object + its leaves, never just the one view entry (Codex #3).
6. **Object cache is opt-in.** Structural providers emit no `canonical-store`; the cache self-selects. No host shape declaration.
7. **No double storage.** Identity representations (`byte-source::canonical`) live only in the object cache; the view never copies them.
8. **View is disposable.** A separate `view`, deleted and recreated on every startup — no crash-detection, no sentinel. It never survives a restart to disagree with the durable object (Codex #5); object and view need no cross-store atomicity.
9. **Schema decoupled from wire.** `omnifs-cache` has no `omnifs-wit` dependency; the durable serde schema is versioned independently; the host owns the wire bridge.
10. **Release-validated leaves.** `op_validate` (not a `debug_assert`) rejects empty logical ids, empty or invalid view leaves, invalidation ids, and oversized validator tokens (Codex #10).
11. **Mount isolation by key prefix.** Cross-mount reads are unrepresentable.

## Wire delta (ADR-0001 §9 ↔ shipped WIT)

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

The object cache (precious, low write rate) is **durable and global**; the view cache (high write rate, disposable) is **non-durable** — a separate fjall keyspace deleted and recreated on every startup. This split is what makes the contention and atomicity findings tractable, with no bespoke crash-detection machinery.

```
<cache-dir>/object               durable, global, survives restart
    objects : "{mount}\x1f{anchor}"    -> { canonical, leaves }   // leaves = reverse set for exact eviction
    paths   : "{mount}\x1f{leaf-path}" -> anchor                  // forward map for the read-path push
<cache-dir>/view                 non-durable: deleted + recreated on every startup
    metadata/content/bulk : "{mount}\x1f{kind}:{path}"[<aux>]
```

- **Object DB: one global keyspace, mount-prefixed keys.** Object writes happen only on a cold fetch, and fjall is safe under concurrent writers, so write contention is minimal (Codex #9). Mount prefix reuses the existing range-scan machinery and keeps keyspace names static.
- **View DB: a separate fjall keyspace, deleted on startup.** No sentinel, no clean-vs-crash distinction — the host removes `view` at startup and reopens it empty, so the view is always cold after any restart and can never survive to disagree with the durable object (Codex #5). The view recomputes from the object with no upstream refetch. Because the view is disposable, object and view need no cross-store atomicity. Ordinary view writes are lock-free; only the dirent-listing merge is a read-modify-write, and it runs as an optimistic transaction (retry on conflict), so merges of different keys never contend (Codex #8/#9).

Budgets: object DB global LRU, no TTL; view LRU; in-memory `mem` tier (moka) over each. Per-mount fairness for the object DB is deferred until a noisy mount is shown to starve a quiet one.

## Tradeoffs accepted

- **Object DB write path (global keyspace).** fjall handles concurrent writers, and object writes happen only on cold fetches, so contention is minimal; large deletes (mount teardown) are chunked so they don't stall an unrelated mount's fetch.
- **View recompute after any restart.** Startup deletes `view`, so the first read of each path after a restart re-renders from the durable object (local, no upstream). Cheaper than the machinery to keep a warm view across clean shutdowns.
- **Large ranged `.raw`.** A `Deferred { Ranged }` object `.raw` still has no `read-file` byte source; remains deferred per ADR-0001.

## Adversarial review (Codex gpt-5.5, accepted findings)

The design was hardened against an adversarial review. Folded in above:

- **#1 (Critical)** push captured before `op_gen` and the rendered result re-cached after a mid-op invalidation → push carries `hit_gen`; the view write is fenced.
- **#2 (Critical)** wrong/stale map silently renders -> `canonical-input` carries the logical id; SDK self-checks before rendering.
- **#3 (Critical)** invalidating a representation path left the object alive -> leaf invalidation cascades to the logical id via the path index.
- **#4 (High)** File-shape breaks prefix deletion -> exact-leaf eviction from the object's reverse leaf set; the "textual-containment means free range-delete" claim is withdrawn.
- **#5 (High)** cross-DB invalidation isn't crash-atomic → the view is a separate `view` deleted on every startup, so no view survives a restart to disagree with the object.
- **#6 (High)** object eviction + refetch -> mixed versions -> a `canonical-store` overwrite evicts the object's prior derived views first.
- **#7 (High)** global generation + tombstone GC unsafe across mounts -> the fence is per-mount.
- **#8 (Medium)** concurrent dirent merge lost-update -> the merge runs as an optimistic transaction inside the cache (`fetch_update` + commit/retry), so a concurrent merge of the same key forces a re-read instead of a lost update.
- **#9 (Medium)** global write-lock contention -> fjall is safe under concurrent writers, so neither store needs a global write lock. Object writes are cold-only and barely contend; ordinary view writes are lock-free, and only the same-key dirent merge serializes (via the optimistic retry above). If a single global `view` keyspace ever contends under load, split it per-mount (same delete-on-startup rule) and chunk large prefix deletes; deferred until shown necessary.
- **#10 (Medium)** `leaves` only `debug_assert`ed → release validation in `op_validate`.

Confirmed fine: structural files under an object dir; dropping `content-type`.

## Historical migration slices

These slices record how the branch reached the current model. They are retained as implementation history, not as remaining work.

0. **Delete `tree-handoffs`** (warm-up; no behavior change).
1. **Consolidate the view cache + rename crate.** The two former view tiers (in-memory `mem` + durable `disk`) merge into one `view::Cache`; the `mem` tier moves out of `FuseFs` into the cache; `omnifs-view` became `omnifs-cache`; module reorg. 3 host import sites + workspace member list.
1a. **Split materialization out to the host.** Move `apply_effects` and `content_type_for` into a host `Materializer`; drop the cache's `protocol` Effects-family DTOs. **Keep the dirent read-modify-write as a transactional cache primitive** (`Store::project`), not a host read-merge-write (#8). Cache surface shrinks to storage primitives.
2. **Wire change.** `canonical-store` now carries `id`, `validator`, `bytes`, and `view-leaves`; `canonical-input` carries `id`, `validator`, and `bytes` (#2); `op_validate` validates view leaves in release (#10). Atomic wire.
3. **Object cache durable + exact map + fences.** Durable global `object` (objects-with-leaves + paths); exact path-to-id lookup replaces the prefix probe; per-mount fence (#7) with `hit_gen` on the push (#1); object overwrite evicts prior derived views (#6); leaf invalidation cascades to the object (#3); SDK id self-check honored (#2). Non-durable `view` deleted + recreated on startup (#5; no sentinel). Behavior-changing live-container validation covered cold `ls` issues -> read two representations (second renders, no refetch); structural sibling read pushes nothing; version advance then re-read a sibling representation serves the new version; kill -9 then restart serves correct recomputed bytes; restart keeps the object.
4. **Docs.** Fold into ADR-0001 §4; retire `cache-architecture.md`; fix CLAUDE.md "Caching model" + "Provider architecture".
