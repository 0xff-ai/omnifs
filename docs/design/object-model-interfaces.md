# Object-model interface contract

Status: implemented contract
Derives from: the object model in `docs/design/architecture.md` §2 (authoritative
spec), the Phase A gate review, and its three deferred-decision proposals
(WIT / redb / SDK) reconciled here.

This document is the **single contract** Tier 1 and Tier 2 build against. Every
record, type, and method below is inlined so an implementer needs nothing else.
When the spec and this doc agree, the spec wins on intent and this doc wins on the
concrete shape (it is the resolution of the spec's deferred decisions).

The gate review returned **GO**. The adversarial pass (D4) found no hole in the
freshness/identity/multikey machinery. The spec needed three doc fixes (applied
separately: SIC-1 object-record fields, SIC-2 `.volatile()` restriction,
NEG-CHANNEL negative wire channel) plus polish; those do not change the model.

## 0. The two orchestrator decisions (settled)

1. **`logical-id` is a structured wire record, not a string.** `kind` plus an
   ordered list of `{name, value}` captures. The host treats the whole record as
   opaque key bytes; it never reads the fields for routing. The `kind|name=value`
   string survives only as a diagnostics `Display`.
2. **The durable object record does not store the fence generation.** Its fields
   are `{ id, canonical: option<{bytes, validator}>, leaves }`. The per-mount fence
   generation and tombstones are in-memory and reset on restart (spec §6.5).

User decisions: negative records use **id-bearing non-error NotFound terminals**;
build cadence is **autonomous to green**.

## 1. `logical-id` encoding and the canonicalization contract

```wit
// provider.wit, types interface
record logical-id {
    kind: string,                 // ObjectKind, e.g. "github.issue"
    captures: list<id-capture>,   // identity captures, DECLARATION order
}
record id-capture {
    name: string,                 // key field name, e.g. "owner"
    value: string,                // normalized Display of the parsed typed value
}
```

**Canonicalization contract (producer = SDK, consumer = host, never re-derived):**

- `captures` are in **declaration order**: the order the key's fields are declared,
  with a `#[flatten]` parent's captures prepended. The macro emits this order
  directly (it walks fields top to bottom); there is no sort step.
- A capture `value` is the **idempotent `Display`** of the parsed typed value.
  Normalization (e.g. GitHub owner case-folding) happens once in `FromStr`; identity
  never uses the raw path segment. The `#[path_captures]` macro emits a generated
  `#[test]` asserting `FromStr`/`Display` round-trips idempotently for a sampled set
  (spec §11).
- `Facet<T>` fields are **excluded** from `captures` (they are route context, not
  identity). Pure-navigation attach-prefix captures (arXiv `category`) never reach
  the key, so they are excluded automatically.

**Host treatment:** opaque. `ObjectId` key bytes are
`mount_bytes ++ 0x1F ++ postcard(logical_id)`. The host forms, compares, stores,
and range-scans (by `mount ++ 0x1F` prefix) these bytes and never destructures
them. Identity collapse (`issues/open/42` vs `issues/all/42` → one object) is **byte
equality** of two `logical-id` records, which holds because both keys produce the
identical `{kind, captures}` in the identical order (`filter` is a `Facet`, dropped).

`MountName` forbids the `0x1F` separator (validated at construction), so the prefix
is unambiguous.

## 2. WIT record changes (literal)

All changes are in `crates/omnifs-wit/wit/provider.wit`, `types` interface. The WIT
package version is `omnifs:provider@0.4.0` (the `0.3.0 → 0.4.0` breaking bump landed).
Host and guest bindgen regenerate together (atomic wire change).

### 2.1 Effects

```wit
record canonical-store {
    id: logical-id,                  // was: anchor: string
    validator: option<version-token>,// was: version  (the conditional-request validator)
    bytes: list<u8>,
    view-leaves: list<string>,       // was: leaves; now FULL absolute paths (see below)
}

record fs-write {
    id: option<logical-id>,          // NEW; Some for object leaves, none for structural
    path: string,
    kind: fs-kind,                   // unchanged: file(file-out) | directory(bool)
}

variant invalidation {
    object(logical-id),              // was: path(string)
    listing(path-or-prefix),         // was: prefix(string)
}
variant path-or-prefix {
    path(string),
    prefix(string),
}

record effects {
    canonical: list<canonical-store>,
    fs: list<fs-write>,
    invalidations: list<invalidation>,
}
```

**`view-leaves` are full absolute paths**, not anchor-relative remainders (the
multikey store spanning `open/42` and `all/42` has no single anchor to be relative
to). They are **every leaf the object indexes** across the current attach prefix
expanded over each finite `Facet`'s `choices()`: the source/canonical leaf, every
derived render, and every warm field. The source/canonical leaf path **is** in
`view-leaves` (so it joins the path→id index and `invalidate object` reaches it),
even though its bytes are never copied into the view cache (served verbatim from the
object cache; spec invariant #7). The index includes the source leaf; the view bytes
exclude it.

### 2.2 The NotFound (negative) channel

NotFound becomes a **non-error, id-bearing terminal** so it can carry the
`logical-id` and be fenced (an `op-result::error` terminal carries no effects; the
error-no-effects rule is `error_returns_do_not_mutate` in `op_validate.rs`).

```wit
// lookup: enrich the existing arm
variant lookup-child-result {
    entry(lookup-entry),
    subtree(tree-ref),
    not-found(option<logical-id>),   // was: not-found
}

// read: NotFound stops being an error
variant read-file-outcome {
    found(read-file-result),
    not-found(option<logical-id>),
}
// op-result arm changes: read-file(read-file-result) -> read-file(read-file-outcome)
```

`some(id)` ⇒ the host stores a fenced negative keyed by that `ObjectId`. `none` ⇒
plain ENOENT, no negative (a structural miss with no object identity). An
`op-result::error(error-kind::not-found)` still exists for routing misses that have
no `logical-id`; the host treats it as ENOENT with no negative.

### 2.3 The warm-read push

```wit
record canonical-input {
    id: logical-id,                  // was: anchor: string
    validator: option<version-token>,// was: version
    bytes: list<u8>,
}
```

The host emits the **stored** `logical-id` (learned from the prior
`canonical-store`), not a path. The SDK self-checks `key.anchor() == input.id`
before rendering; on mismatch it treats the push as absent and does `load(None)`
(this self-check landed at `router/object.rs`: the warm-read path verifies the
host-pushed id against the route-derived `Key::anchor` and falls through to `load`
on mismatch).

## 3. SDK core types (Rust)

In `crates/omnifs-sdk`. Bodies may be stubbed at the Tier-0 freeze; signatures are
frozen.

```rust
// Identity ---------------------------------------------------------------
pub struct ObjectKind(pub &'static str);              // existing

pub struct LogicalId {
    pub kind: ObjectKind,
    pub captures: Vec<(&'static str, String)>,        // declaration order, normalized Display
}
impl From<LogicalId> for wit::LogicalId { /* 1:1 */ }
impl From<wit::LogicalId> for LogicalId { /* 1:1 */ }
impl fmt::Display for LogicalId { /* "kind|name=value|..." — diagnostics only */ }

// The conditional-request validator. Renames the file-attributes VersionToken at
// the object layer; the wire field is `validator`.
pub type Validator = VersionToken;                    // keep VersionToken as the base newtype

pub struct Canonical { pub bytes: Vec<u8>, pub validator: Option<Validator> }
pub enum Load<T> { Fresh { value: T, canonical: Canonical }, Unchanged, NotFound }
impl<T> Load<T> { pub fn fresh(value: T) -> Self { /* Fresh, validator None */ } }

// The data ---------------------------------------------------------------
pub trait Object: Serialize + DeserializeOwned + Sized {
    type Key: Key<Object = Self>;
    fn kind() -> ObjectKind;                          // macro
    fn canonical_content_type() -> ContentType;       // macro, default Json
    fn default_stability() -> Stability { Stability::Mutable }   // macro from #[object(stability=)]
    fn parse_canonical(bytes: &[u8]) -> Result<Self> {           // default serde-JSON; override for Atom/XML
        serde_json::from_slice(bytes).map_err(|e| ProviderError::invalid_input(format!("canonical parse: {e}")))
    }
}

// Identity captures: macro-emitted (O-independent; the macro cannot see Object).
pub trait IdentityCaptures {
    fn identity_captures(&self) -> Vec<(&'static str, String)>;  // declaration order, Facet-excluded
}

// The identity ------------------------------------------------------------
pub trait Key: FromCaptures + IdentityCaptures + Sized {
    type Object: Object<Key = Self>;
    type State;
    async fn load(&self, cx: &Cx<Self::State>, since: Option<Validator>)
        -> Result<Load<Self::Object>>;
    // DEFAULTED here (composes kind + identity_captures), so the macro needn't see Object:
    fn anchor(&self) -> LogicalId {
        LogicalId { kind: Self::Object::kind(), captures: self.identity_captures() }
    }
}

// Representations ---------------------------------------------------------
pub trait Format { const CT: ContentType; }           // ZST markers: Json, Markdown, Atom, Octet, Yaml, Html, Diff
pub trait Representable<F: Format> { fn represent(&self) -> Vec<u8>; }

// Erased render table: ContentType -> fn(&[u8]) -> Result<Vec<u8>>, built once per
// route from the automatic source leaf (verbatim) + the representations tuple.
// Function pointers, not boxed closures.
type RenderFn = fn(&[u8]) -> Result<Vec<u8>>;
pub struct RenderTable { source_ct: ContentType, renders: Vec<(ContentType, RenderFn)> }
impl RenderTable {
    fn serve(&self, ct: ContentType, canonical: &[u8]) -> Result<Vec<u8>>; // verbatim if source_ct, else invoke
}

// Route context excluded from identity. Deref so handlers read *key.filter.
pub struct Facet<T: PathSegment>(pub T);
impl<T: PathSegment> Deref for Facet<T> { /* &T */ }
```

`o.representations(stem, renders)` declares only derived renders as a tuple of
`Format` markers; the source leaf `<stem>.<ext>` is automatic
(`ext = canonical_ct.extension().unwrap_or("raw")`). Each render `F` adds a
`RenderTable` entry `|canon| O::parse_canonical(canon)?.represent::<F>()` keyed by
`F::CT`. Duplicate CT or duplicate extension is a build-time error. This **replaces**
`Object::render`, `render_representation`, and `ObjectMeta::representations()`.

## 4. SDK registration & multikey internals

In `crates/omnifs-sdk/src/router`. Replaces `r.bind`, `Section`/`SectionDirRoute`/etc,
`Router<S, R>`'s `R` param, and `build_path`.

```rust
pub struct Router<S> { /* objects, dirs, files, treerefs, leaf-claim index */ }

// detached handle, replayable at N attach prefixes:
pub fn object<O: Object>(template: &'static str, block: impl FnOnce(&mut DirObjectBlock<O>) -> Result<()>)
    -> ObjectHandle<O>;
impl<S> Router<S> {
    pub fn object<O: Object>(&mut self, template, block) -> Result<&mut Self>;       // single-attach sugar
    pub fn file_object<O: Object>(&mut self, template, block) -> Result<&mut Self>;  // File-shape; block has no file/dir
    pub fn attach<O: Object>(&mut self, prefix: &str, handle: &ObjectHandle<O>) -> Result<&mut Self>;
    pub fn dir(&mut self, template) -> DirRoute<'_, S>;     // .handler / .dirs::<T>() / .files::<T>()
    pub fn file(&mut self, template) -> FileRoute<'_, S>;
    pub fn treeref(&mut self, template) -> TreeRefRoute<'_, S>;
    pub fn seal(&self) -> Result<()>;    // one-path-one-id check; called by generated initialize() after start()
}

// Dir-shape block (File-shape omits file/dir):
impl<O: Object> DirObjectBlock<O> {
    fn representations<R: RenderSet<O>>(&mut self, stem: &'static str, renders: R) -> Result<&mut Self>;
    fn file(&mut self, name) -> LeafBuilder<'_, O>;   // .project(O::method) | .handler(Key::method) | .lazy()/.immutable()/.mutable()/.volatile()
    fn dir(&mut self, name) -> ChildBuilder<'_, O>;   // .handler(Key::method)
    fn when(&mut self, pred: fn(&O::Key) -> bool) -> Result<&mut Self>;
}
```

**Multikey `view-leaves` expansion.** At `canonical-store` time the SDK emits the
full leaf paths for the current request's matched concrete anchor, expanded over
each finite `Facet`'s `choices()`. Mechanism: a macro-emitted `FacetAxis { seg_index,
choices }` table (from `O::Key`'s `Facet` fields crossed with the route `Pattern`)
drives **segment substitution** on the already-resolved concrete anchor path (not a
reverse-routing renderer). For `StateFilter = {open, all}` a store from `open/42`
emits both `…/open/42/<leaf>` and `…/all/42/<leaf>` for every declared leaf. Unbounded
/ pure-nav prefix captures (arXiv `category`) have no finite `choices()` and are
**not** expanded; they index lazily on first visit (spec §6.1).

**One-path-one-id (`seal`).** Build the `LeafClaim` set once (anchor pattern + every
rep/field/child leaf pattern + structural routes) and run pairwise
`omnifs_core::path::Pattern::is_ambiguous_with` (already present, currently unused) over
it. Overlap ⇒ `start()`-time error surfaced through `initialize()`. The same
`LeafClaim` set feeds the multikey expansion and the render table.

## 5. `#[path_captures]` / `#[object]` macro changes

`crates/omnifs-sdk-macros`.

- `#[path_captures]`: gains `Facet<T>` detection and `#[flatten]` parent embedding
  (modelled on the existing Option detection). Emits `FromCaptures` (Facet-aware) and
  `IdentityCaptures::identity_captures()` (declaration order, Facet-excluded, flatten
  prepends parent), plus the generated round-trip `#[test]`. Does **not** emit
  `anchor()` (defaulted on `Key`).
- `#[object(kind = "...", key = KeyType, canonical = ..., parse = ..., stability = ...)]`:
  emits `Object::Key`, `kind`, `canonical_content_type`, `default_stability`
  (default `Mutable`), and optional custom `parse_canonical`. The old
  `#[omnifs(field(...))]` / `represents` / `repr_stem` / `routes_type` attributes are
  **removed**; fields are hand-written `&self` methods referenced by `.project`, and
  renders are hand-written `Representable<F>` impls.
- `#[provider]`: `start` returns `Result<State>` (drop the `Routes` tuple); the
  generated `initialize()` calls `router.seal()?` after `start()`.

## 6. Cache contracts (`crates/omnifs-cache` + host wiring)

### 6.1 Durable redb (`object.redb`)

Two tables, byte keys (the v1 `&str` keys migrate to `&[u8]` because the id is now
structured).

```
objects : key = mount ++ 0x1F ++ postcard(logical-id)
          val = postcard({ schema: u8, id: LogicalId,
                           canonical: Option<{ bytes: Vec<u8>, validator: Option<String> }>,
                           leaves: Vec<String> })           // leaves = full scoped paths
paths   : key = mount ++ 0x1F ++ full-path-string
          val = mount ++ 0x1F ++ postcard(logical-id)       // forward index, learned from effects
```

- `canonical: None` represents a **preload-only** object (id-bearing `fs-write`, no
  `canonical-store`) or a **capacity-evicted** canonical whose path↔id entries were
  kept. Both still resolve a path to an id; a read then does `load(None)`.
- `leaves` is the durable **alias set** (every full path indexed for this id).
- No `generation` field (SIC-1). The fence lives in `Store` (§6.3).

### 6.2 Ephemeral view (`view.redb`, deleted + recreated on startup)

Existing per-record store (`Lookup`/`Attr`/`Dirents`/`File`) gains a per-leaf
**freshness stamp** shared by all RecordKinds of one path:

```
freshness : key = mount ++ 0x1F ++ path
            val = { expires_at: Option<u64-millis>, generation: u64 }
```

Written together with a leaf's records and checked on read: a leaf is *fresh* when
`now < expires_at` (or `expires_at = None` for Immutable). `expires_at` is
Stability-derived at write time: Immutable ⇒ `None`; Mutable ⇒ `now + ttl` (host
config, default a few seconds); Volatile ⇒ `now` (TTL 0). The Store reads the host
clock; this is the only clock dependency.

### 6.3 In-memory per-mount state (in `Store`, reset on restart)

```rust
generation: AtomicU64,                              // per-mount fence clock
tombstones: DashMap<ObjectIdBytes, u64>,            // id -> generation last invalidated
negatives:  DashMap<ScopedPath, Negative>,          // path -> { id: Option<LogicalId>, expires_at, as_of_gen }
neg_by_id:  DashMap<ObjectIdBytes, HashSet<ScopedPath>>,  // for invalidate object(id) to clear negatives
```

Negatives are keyed by **path** (a NotFound path was never indexed, so it cannot be
found via path→id) and tagged with the id, so `invalidate object(id)` clears every
negative for that id. In-memory: a NotFound re-dispatches after restart, which is
correct.

### 6.4 `Store` method contract

```rust
impl Store {
    // reads
    fn view_get(&self, path, kind, aux) -> Option<Record>;          // checks freshness
    fn cached_canonical_for(&self, path) -> Option<CanonicalInput>; // path -> id -> {id, bytes, validator}; None if no canonical
    fn negative_for(&self, path) -> Option<Negative>;               // live, unfenced negative -> ENOENT w/o dispatch
    fn current_generation(&self) -> u64;

    // writes (all fenced against op_gen by the id's tombstone)
    fn put_canonical(&self, id: &[u8], bytes: Vec<u8>, validator: Option<String>, view_leaves: &[String], op_gen: u64) -> bool;  // id is host-opaque bytes
    fn put_canonical_batch(&self, entries: Vec<CanonicalBatchEntry>, op_gen: u64);  // one-write batch path; fenced per-entry
    fn put_index_only(&self, id: &LogicalId, view_leaves: &[String], op_gen: u64) -> bool;  // id-bearing fs-write (preload)
    fn put_negative(&self, path: &str, id: Option<&LogicalId>, op_gen: u64) -> bool;
    fn cache_view_leaf(&self, path, id: Option<&LogicalId>, record, stability, op_gen) -> bool; // sets freshness
    fn merge_projected_dir(&self, dir, children, exhaustive);       // transactional dirent RMW (unchanged)

    // invalidation
    fn delete_object(&self, id: &LogicalId);     // the ONLY index remover: canonical+validator+rendered+alias set+negatives
    fn delete_listing_path(&self, path: &ProtocolPath);     // VIEW-ONLY: exact dirent/listing entry; never touches object canonicals or paths index
    fn delete_listing_prefix(&self, prefix: &ProtocolPath); // VIEW-ONLY: dirent/listing entries under a prefix

    // eviction
    fn capacity_evict(&self, id: &LogicalId);    // drop canonical bytes+validator+rendered; MAY keep paths index (-> canonical:None)
    fn write_fenced(&self, id_or_path, op_gen) -> bool;
}
```

### 6.5 Mutation semantics (the resolved conflicts)

- **`put_canonical` overwrite UNIONS aliases.** It replaces `canonical` bytes +
  validator, **unions** the incoming `view-leaves` into the row's `leaves` set,
  **keeps** all `paths` index entries, and drops only the **rendered view bytes** for
  the id's known leaves (version coherence). It does **not** remove `paths` rows
  (current `object::Cache::store` deletes prior leaf rows — that REPLACE behavior is
  removed). A lazily-added arXiv-category alias therefore survives an overwrite and
  re-renders from the in-hand canonical.
- **`delete_object(id)` is the only index remover.** Full eviction: canonical,
  validator, rendered bytes, alias set, **all** `paths` rows for the id, and negatives.
- **`delete_listing_path` / `delete_listing_prefix` are VIEW-ONLY.** They evict
  dirent/listing records at the path or under the prefix in the view cache and must
  **not** call object eviction and must **not** remove `paths`↔id entries.
- **`capacity_evict(id)`** drops canonical bytes + validator + rendered atomically
  (so the object-miss branch always means "no canonical and no validator," never a
  stranded validator); it may keep `paths` rows (→ `canonical: None`), so a re-read
  finds the id, misses the canonical, and does `load(None)`.
- **The fence covers every object-derived write.** `put_canonical`,
  `put_index_only`, `put_negative`, and `cache_view_leaf` are admitted only if no
  tombstone for the id is newer than `op_gen`. Id-less structural `fs-write`s carry no
  id and are out of fence scope by construction.
- **`canonical-beats-preload`.** A `put_index_only` (preload `fs-write`) for an id
  that already has `canonical: Some` is dropped (the canonical is authoritative).
- **Runtime one-path-one-id.** At materialization the host rejects any effect
  mapping an already-indexed path to a different id (a provider bug), keeping the
  forward map single-valued in operation.

## 7. Read path (single-shot, no suspend)

`read_file(path, ct, cached_canonical)` is one call:

1. **View fresh** (`view_get` returns a leaf before its deadline) ⇒ serve rendered
   bytes, no dispatch.
2. Else consult `negative_for(path)`: live + unfenced ⇒ ENOENT, no dispatch.
3. Else `cached_canonical_for(path)`:
   - **Some** ⇒ push `canonical-input { id, bytes, validator }`; SDK self-checks
     `key.anchor() == id`, renders without upstream; if the leaf is Mutable and
     expired it may revalidate with `validator` (`load(Some(v))`): `304 → Unchanged`
     re-render + reset deadline; `200 → Fresh` emit overwrite `canonical-store`.
   - **None** ⇒ dispatch `read_file(path, ct, None)`; SDK routes, computes the id,
     `load(None)`, renders, returns bytes + `canonical-store { id, … }` (or
     `not-found(Some(id))`). Host stores under the id and indexes the leaves.

The host never suspends mid-read; identity collapse comes from the multikey store
(§4), not a suspend-to-ask. **Volatile leaves bypass singleflight** (they are ranged
sources): each read issues its own ranged read.

**Singleflight** keys on `ObjectId`: concurrent reads of any leaf of one object
attach to one in-flight `load`; the load captures its read generation at start and is
fenced on completion. A mid-flight `invalidate object(id)` makes the load lose; all
waiters discard and re-load at the new generation (no waiter renders rejected bytes or
resets a deadline).

## 8. Acceptance invariant → mechanism (where each §14 test lands)

| # | Invariant | Mechanism | Test tier |
|---|---|---|---|
| 1 | identity collapse | multikey `view-leaves` over Facet `choices()` (§4) | Tier 2 (github) |
| 2 | mount isolation | `mount ++ 0x1F` key prefix (§6.1) | Tier 1 (cache) |
| 3 | canonical is raw | source leaf served verbatim from object cache (§2.1) | Tier 2 (github) |
| 4 | single-item-equivalence | eager vs lazy fields; sampling test | Tier 2 (github) |
| 5 | preload coherence | overwrite unions/keeps index, drops rendered bytes; canonical-beats-preload (§6.5) | Tier 1 (cache) |
| 6 | object-miss revalidation | `capacity_evict` drops validator; `load(None)` (§7) | Tier 1 (host) |
| 7 | no stranded 304 | validator evicted with canonical (§6.5) | Tier 1 (cache) |
| 8 | fence covers preload+negatives | fence on `put_index_only`/`put_negative` (§6.5) | Tier 1 (cache) |
| 9 | singleflight loser | id-keyed singleflight + generation fence (§7) | Tier 1 (host) |
| 10 | atomic leaf freshness | shared freshness stamp per path (§6.2) | Tier 1 (cache) |
| 11 | revalidation dedup | id-keyed coalescing window (§7) | Tier 1 (host) |
| 12 | cold dispatch only | no path parsing; learned index (§6.1) | Tier 1 (host) |
| 13 | structural gating | enforced-disjoint `load` (Issue/PR) | Tier 2 (github) |
| 14 | invalidation intent | `delete_object` vs view-only `delete_listing_path`/`delete_listing_prefix` (§6.5) | Tier 1 (cache) |
| 15 | representation dispatch | `RenderTable` (§3); dup-CT build error | Tier 1 (sdk) |
| 16 | negative records | id-bearing NotFound terminal + in-memory negatives (§2.2, §6.3) | Tier 1 (host) |
| 17 | attach symmetry | path-independent `LogicalId` (§1) | Tier 2 (arxiv) |
| 18 | route-map visibility | `seal` / registered routes | Tier 1 (sdk) |
