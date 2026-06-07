# Object-shaped providers (SDK model)

Status: implemented

This spec defines the provider model: external resources are modeled as typed
**objects** addressed by typed **keys**, with all routing and projection declared
in `start()`. It supersedes the route-shaped handler model and the
legacy `r.bind`/`#[object]` surface, and it amends the object-cache design
(`object-cache-primary.md`) where noted, most substantially by making the
host byte-level and path-keyed while the provider owns all path→object mapping
and supplies the logical identity in effects.

The spec is the product of a design interview and two adversarial review rounds;
the decision log and the review-resolution log at the end record each fork.

## 1. Goals and the problem

Providers currently describe services as route handlers that each parse captures,
fetch upstream data, list children, project files, sibling effects, and validate
child paths. That spreads one conceptual resource across two models (route-shaped
and object-shaped) and duplicates field-projection and cache-identity policy. The
GitHub item is both, and `issues/open/42` and `issues/all/42` cache the same
upstream object twice because the cache anchor is the full visible path.

The model separates four concerns and gives each one home:

- **Identity**: which captures name the canonical upstream resource (plus the
  mount it was read through).
- **Route context (Facets)**: captures that affect navigation/validation but not
  identity.
- **Canonical form**: the raw upstream bytes the host caches, with its validator.
- **Projection surface**: representations, field leaves, and child resources.

The route map stays fully visible in `start()`; objects hold data plus logic.

## 2. The two types per object

Each object is two Rust types with a 1:1 link:

- The **Object** (the data): a `#[derive(Serialize, Deserialize)]` struct carrying
  the parsed canonical, its field-projection methods, its `Representable<F>` render
  impls, and `parse_canonical`. It is "the object" because it is what the object
  cache stores (as bytes). Example: `Issue`.
- The **Key** (the identity): a captures struct carrying the path captures, with
  `anchor()` (the provider-local logical identity), `load` (fetch), and the
  child-resource handler methods. Example: `IssueKey`.

```rust
// the data
#[omnifs_sdk::object(kind = "github.issue", key = IssueKey)]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Issue { number: u64, title: String, body: Option<String>, state: String, user: Option<User>, updated_at: Option<String> }

impl Issue {
    fn title(&self) -> FileContent { FileContent::text(&self.title) }
    fn body(&self)  -> FileContent { FileContent::markdown(self.body.as_deref().unwrap_or("")) }
    fn state(&self) -> FileContent { FileContent::text(&self.state) }
    fn user(&self)  -> FileContent { FileContent::text(self.user.as_ref().map_or("", |u| &u.login)) }
}
impl Representable<Markdown> for Issue { fn represent(&self) -> Vec<u8> { /* … */ } }

// the key
#[omnifs_sdk::path_captures]
struct IssueKey { owner: OwnerName, repo: RepoName, filter: Facet<StateFilter>, number: u64 }

impl Key for IssueKey {
    type Object = Issue;
    async fn load(&self, cx: &Cx<State>, since: Option<Validator>) -> Result<Load<Issue>> {
        let repo = RepoId::new(&self.owner, &self.repo);
        let (item, canonical): (Issue, Canonical) =
            cx.github_load(format!("/repos/{repo}/issues/{}", self.number), since).await?;
        if item.is_pull_request() { return Ok(Load::NotFound); }   // enforced disjointness (12.1)
        Ok(Load::Fresh { value: item, canonical })
    }
}

impl IssueKey {
    async fn comments(&self, cx: DirCx<State>) -> Result<DirProjection> { /* … */ }
}
```

### 2.1 Trait surface

```rust
trait Object: Serialize + DeserializeOwned + Sized {
    type Key: Key<Object = Self>;
    fn kind() -> ObjectKind;                          // macro
    fn canonical_content_type() -> ContentType;       // macro (default Json)
    fn parse_canonical(bytes: &[u8]) -> Result<Self>  // default serde-JSON; override for Atom/XML
        { serde_json::from_slice(bytes).map_err(invalid) }
}

trait Key: FromCaptures + Sized {
    type Object: Object<Key = Self>;
    fn anchor(&self) -> LogicalId;                    // macro: provider-local logical identity, Facets excluded
    async fn load(&self, cx: &Cx<S>, since: Option<Validator>) -> Result<Load<Self::Object>>;
}

trait Representable<F: Format> { fn represent(&self) -> Vec<u8>; }

struct Canonical { bytes: Vec<u8>, validator: Option<Validator> }   // the raw upstream body + its validator (`Validator` = the conditional-request token named `version`/`VersionToken` in file-attributes.md)
enum Load<T> { Fresh { value: T, canonical: Canonical }, Unchanged, NotFound }
```

`load` is state-aware (`Cx<State>`, previously `Cx<()>`) and receives the cached
`Validator` so a provider can issue a conditional request. Outcomes:

- `Fresh` carries the parsed value plus the **raw upstream response bytes and
  validator** as `Canonical`. The provider supplies these (it owns the callout and
  knows which response is the canonical), and the SDK emits them as the
  `canonical-store` effect (§7.1). The raw bytes are what makes `cat item.json`
  byte-equal the single-item GET. `Load::fresh(value)` is sugar when there is no
  validator.
- `Unchanged` is the 304 outcome. It is **only** legal when the host pushed a
  canonical for this read; the host never issues a conditional request without
  present canonical bytes (§6.1), so a stranded `Unchanged` is an SDK error.
- `NotFound` is cacheable absence; it produces a fenced negative record (§7.5).
- Transient/transport failures are `Err` and cache nothing.

## 3. Identity, in two layers

Identity is split across the host/provider boundary (§3.2):

```
LogicalId = (Object::kind(), { identity-capture name = normalized-value })   // computed in the provider
ObjectId  = (mount, LogicalId)                                               // formed by the host
```

The provider computes `LogicalId` from the key's non-`Facet` fields (it is the
only side with the typed key) and emits it in every effect. `Key::anchor()`
returns this `LogicalId`. The host prefixes the mount to form the cache key
`ObjectId`, treats `LogicalId` as opaque, and never parses a path to recover it
(§7.2). Three properties:

- **Mount-scoped (host).** `ObjectId` includes `mount`, so two mounts of one
  provider, or two providers reusing a kind string, never share canonical bytes.
  This is a security boundary: two GitHub mounts with different credentials see
  different private data for the same `owner/repo/number`, so their canonicals
  must not be shared. Object, view, reverse-index, negative, and tombstone keys
  are all mount-prefixed (consistent with `object-cache-primary.md`).
- **Path-independent (provider).** The same `LogicalId` is produced regardless of
  attach point or Facet. arXiv `/papers/2401.x` and `/categories/cs.AI/papers/2401.x`
  both yield `arxiv.paper|paper=2401.x` (`category` is not a key field; `filter`-
  style Facets are dropped). GitHub `/o/r/issues/open/42` and `/o/r/issues/all/42`
  both yield `github.issue|owner=o|repo=r|number=42` (`filter` is a `Facet`).
- **Normalized (provider).** A capture's contribution is its canonical form: the
  `Display` of the parsed typed value, where normalization (e.g. GitHub owner
  case-folding) happens once in `FromStr`. Identity never uses the raw segment
  string. The `Key` contract requires each identity field's `FromStr`/`Display`
  round-trip to be idempotent; the SDK ships a generated round-trip test per key
  (§11) rather than a macro-level proof.

Identities from different `kind`s are always distinct, so structurally-disjoint
families (GitHub Issue vs PullRequest, §12.1) never collide and never need linking.

### 3.1 Facets

A capture parsed and available to handlers (and to child keys) but excluded from
identity is wrapped `Facet<T>` (with `Deref`, so handlers read `*key.filter`).
Identity = every key field not wrapped in `Facet`. The wrapper is type-enforced; a
mis-tag is a type-level fact, not a silent annotation. Facets remain key fields,
so children that flatten the parent key (§4.4) can read them. Captures present
only in an attach prefix and absent from the key (arXiv `category`) are excluded
from identity automatically; §4.5 governs which captures may live in a prefix.

### 3.2 The host/provider boundary

This is the load-bearing rule the rest of the spec depends on.

> **The host is byte-level and path-keyed. The provider owns all path→object and
> path→`LogicalId` mapping. The host learns path↔id only from effects; it never
> parses a path.**

Consequences:

- The host serves and caches **by path** (view cache) and **by `ObjectId`** (object
  cache). It maps a path to its `ObjectId` only via an **exact index it built from
  prior effects** (§7.2), never by re-deriving captures.
- A path the host has no index entry for is, by definition, a **cold miss**: the
  host dispatches the FUSE op (`lookup_child` / `list_children` / `read_file`) to
  the provider with the path bytes, and the provider's router parses it, computes
  the `LogicalId`, loads, renders, and returns the result **plus effects carrying
  that id**. The host then stores under `(mount, id)`, indexes the materialized
  view-leaf paths → id, and serves.
- The provider is therefore the only component that ever turns a path into an
  object. It does so transparently while handling the op, and **teaches** the host
  the mapping through the id in the effects.

The host needs no typed knowledge of any provider's routes. Everything coherence
needs (`path→id`, `id→paths`, the validator, the canonical bytes) is either
learned from effects or supplied per-op by dispatch.

**The read invariant: the host calls the provider with the cache, never suspends.**
A read is a single `read_file(path, cached_canonical)` call; the host finds
`cached_canonical` in its object cache via the path→id index and pushes it, or
pushes `None`. The provider never suspends mid-read to ask the host for a cached
object — that is a hard invariant. Identity collapse across alias paths (the
original motivating bug: `/issues/open/42` vs `/issues/all/42` sharing one
canonical) is achieved instead by making `canonical-store` **multikey** (§7.1):
one store associates the canonical with every alias path the SDK can enumerate, so
a later read of a sibling alias finds the id already indexed and the host pushes
the shared canonical with no refetch.

## 4. Registration: `start()` is the whole map

`fn start(config: Config, r: &mut Router<State>) -> Result<State>`. No `Routes`
type param and no `build_path`; reverse routing is unnecessary because the host
never needs `id→path` outside the reverse index (§7.2).

### 4.1 The object block

`r.object::<Issue>(template, |o| { … })?` (Dir-shape) or
`r.file_object::<Cfg>(template, |o| { … })?` (File-shape). The closure is the
object's entire subtree, in source order, and returns an `ObjectHandle<Issue>` (a
local value usable with `attach`). Verbs:

- `o.representations("item", (Markdown,))?` — the source/canonical leaf is automatic
  (`<stem>.<ext>`, §8); the tuple lists derived renders, each adding
  `Self: Representable<F>`. `()` declares no renders (source leaf only).
- `o.file("title").project(Issue::title)?` — a **warm** field leaf, projected from
  the loaded object via the named method (returns `FileContent`).
- `o.file("body").project(Issue::body).lazy()?` — `.lazy()` excludes the field from
  list preload (loaded on first direct read). Default is eager, which is an
  assertion of single-item-equivalence (§4.3).
- `o.dir("comments").handler(IssueKey::comments)?` — a **fetch** child (separate
  upstream call), keyed by its own key. `.handler` always takes a key method;
  `.project` always takes an object method.
- `o.file("comments/{idx}").handler(IssueCommentKey::read)?` — a parameterized
  child; `IssueCommentKey` embeds the parent via `#[flatten]` (§4.4).
- `.immutable()` / `.mutable()` valid on any leaf (default inherited from the
  object); `.volatile()` valid **only** on a ranged `.handler` leaf (a live/blob
  source). `.volatile()` on a `.project` field leaf or an `o.representations`
  render is a registration-time error, because a Volatile leaf must be a ranged
  source (file-attributes.md, Legal combinations), never a whole-object render (§6.4).
- `.when(|k: &IssueKey| pred)?` — a general context-gate: hides a child from
  listings and ENOENTs reads when `pred` is false. (GitHub's `diff` instead uses
  the two-object split, §12.1, but `.when` remains available.)

Everything **under** the object's path is in the block; everything at **other**
paths (collections, sibling objects, static enumerations) is a flat statement.

### 4.2 Static enumerable directories

A directory whose children are a finite typed set is declared explicitly, with the
verb stating the node kind (never inferred from `choices()`):

```rust
r.dir("/{owner}/{repo}/issues").dirs::<StateFilter>()?;  // open/all, as directories
```

`.dirs::<T>()` / `.files::<T>()` list `T::choices()` as the stated kind.

### 4.3 Collections and single-item-equivalence

A directory whose children require an upstream call is a flat `r.dir(...)` whose
key-method handler returns `ObjectListing<O>`:

```rust
impl IssueListKey {  // /{owner}/{repo}/issues/{filter}
    async fn list(&self, cx: DirCx<State>) -> Result<ObjectListing<Issue>> {
        let page = list_items(&cx, &self.owner, &self.repo, *self.filter).await?;
        Ok(ObjectListing::Objects(
            page.items.into_iter().map(|i| (self.item(i.number), i)).collect(),  // (IssueKey, Issue)
            page.exhaustive,
        ))
    }
}
```

`ObjectListing<O>` is `Objects(Vec<(O::Key, O)>, exhaustive)` (full rows; the SDK
preloads eager fields) or `Names(Vec<String>, exhaustive)` (ids only; enumerate, no
preload, e.g. arXiv category). Returning `(Key, Object)` pairs lets the SDK read
the `LogicalId` from `key.anchor()` and the view path from the key, with no path
inference. `IssueListKey` is distinct from `IssueKey` (no `number`).

**Single-item-equivalence.** A list row is a re-serialized sub-object of a list
response, never the raw single-item body, so it never becomes a canonical (§6.3).
A field may be **eager** (preloaded from the row) only if its list-row projection
is byte-identical to its single-item projection. The SDK cannot verify this; it is
a provider invariant. Any field that differs by media type, truncation, or
permission must be `.lazy()` (GitHub `body` is truncated in lists). Invariant test
#4 (§14) samples it: a field's cold-read bytes must equal its preloaded bytes.
Eager is therefore a correctness assertion, not just a perf hint. This governs
**fields only**: `item.json` and other representations are never preloaded, so
invariant #3 (`item.json` byte-equals the single-item GET) holds unconditionally. A
preloaded field's freshness during its window is bounded by its Mutable deadline
and the equivalence assertion; a field that can diverge from the single item must
be `.lazy()`.

### 4.4 Children and nesting

A child handler is a `&self` method on the child's own key, which embeds the parent
key via `#[flatten]` so parent captures (identity **and** Facets) are declared once
and are all available:

```rust
#[omnifs_sdk::path_captures]
struct IssueCommentKey { #[flatten] item: IssueKey, idx: u64 }
impl IssueCommentKey { async fn read(&self, cx: Cx<State>) -> Result<FileProjection> { /* self.item.*, self.idx */ } }
```

Every handler takes its own full key as `&self`; there is no "tail captures"
concept. A child that needs a capture must have it in its key (via `#[flatten]`,
which carries Facets, or as its own field). A child cannot read an attach-prefix
capture absent from the key; context a child needs must be promoted to a key
`Facet`, not left as a discarded prefix capture (§4.5). `#[flatten]` is new macro
work; the current `#[path_captures]` parses flat fields only.

### 4.5 Detached routes, attach, and prefix captures

A reusable subtree is defined once and attached at N prefixes. No `section`, no
`mount`, no privileged "home":

```rust
let papers = object::<Paper>("/{paper}", |o| { /* representations, versions, pdf, … */ });
r.attach("/papers", &papers)?;
r.attach("/categories/{category}/papers", &papers)?;
```

`attach` dispatches a path under the prefix by validating the prefix captures and
resolving the suffix against the subtree. Because `LogicalId` is path-independent,
both attaches collapse to one cache entry. `r.object::<O>(full_path, closure)` is
single-attach sugar.

**Attach-prefix capture rule.** A prefix capture is either:

- **Identity-bearing**: a field of the object's key. Then it must appear in every
  attach prefix (or the detached template) with the same meaning and participates
  in identity. An attach whose prefix cannot supply an identity field is a
  registration error.
- **Pure navigation**: absent from the key (arXiv `category`). Validated (a sibling
  route supplies its type, otherwise a safe segment) and discarded; never reaches
  identity or any handler.

Multi-attach at structurally different prefixes is sound only when the differing
captures are pure-navigation. GitHub is single-attach, so owner/repo sit in the key.

## 5. Handlers, uniformly

Captured routes have their handler as a `&self` method on the route's key
(`IssueKey::comments`, `DomainRecordKey::read`, `IssueListKey::list`, `Key::load`);
capture-less routes use free functions `async fn(cx)` (`root_list`). This holds for
object and non-object providers alike. DNS never calls `r.object` but its captured
handlers are still key methods.

## 6. Caching and freshness

Three host caches, all mount-scoped, all governed by §3.2 (the host serves by path,
dispatches misses, learns ids from effects). Note: this design amends the "no TTLs" language in `object-cache-primary.md`: view leaves carry a Stability-derived deadline (§6.2); the object cache itself is deadline-less.

- **Object cache**: durable primary. Stores `{ canonical bytes, validator,
  alias set }` keyed by `ObjectId`, where the alias set is the object's `id → {alias paths}` mapping (§7.2, durable). The per-mount fence generation is not a field of this record; it is in-memory and reset on restart (§6.5). No time expiry; evicted by capacity or invalidation. The validator lives **with** the bytes and is evicted with them.
- **View cache**: derived rendered leaves and dirents, keyed by view path. A view
  leaf's records share **one atomic deadline** (§6.2). Evicted by expiry, capacity,
  or invalidation.
- **Blob cache**: large binary by handle (+ archive trees), unchanged.

### 6.1 The read path (cold vs warm dispatch)

A view leaf is *fresh* (in view cache, before its deadline), *expired* (in view
cache, Mutable past deadline), or *absent* (view miss). Reading `…/issues/all/42/title`:

1. **Fresh** → serve the rendered bytes. No dispatch, regardless of Stability (the
   deadline bounds Mutable staleness).
2. **Expired or absent** → the host consults its path→id index (§7.2):
   - **Index hit** (path seen before): the host has `ObjectId`. Object cache hit ⇒
     it pushes the canonical to the provider: `read_file(mount, path, ct,
     cached_canonical: Some(bytes, validator))`. The provider renders from the
     pushed bytes with no upstream call; if the leaf is Mutable and expired it may
     revalidate with the pushed validator (`load(Some(v))`): `304 → Unchanged`
     re-render from those bytes and reset the deadline; `200 → Fresh` emit a new
     `canonical-store` (overwrite eviction, §7.2) and re-render. Object cache miss
     (canonical capacity-evicted) ⇒ the validator went with it, so the host pushes
     `None` and the provider does a full `load(None)`.
   - **Index miss** (cold path): the host dispatches `read_file(mount, path, ct,
     None)`. The provider routes, computes the `LogicalId`, `load(None)`, renders,
     and returns bytes + `canonical-store { id, … }`. The host stores under
     `(mount, id)` and indexes the leaves.

This closes the stranded-304 case (object miss ⇒ `since = None` ⇒ full fetch) and
the preload-only case (a preloaded leaf with no canonical is an object-cache miss
that takes the full-load branch on first expiry, then is cheap thereafter).

**Cross-prefix identity collapse.** Because `canonical-store` is multikey (§7.1),
the first read of `/issues/open/42/title` indexes both the `open` and `all` alias
leaves for `issue:42`, so a subsequent `/issues/all/42/title` is an index hit
served from the shared canonical — one upstream load, not two (invariant #1).
Aliases the SDK cannot enumerate up front (an unbounded prefix such as arXiv
`category`, which is a discarded prefix capture with no finite choice set) are
added lazily: the first read through a new prefix is a cold dispatch that re-stores
with that alias added, costing one refetch, after which it is indexed. Two concurrent cold reads of a not-yet-indexed alias each issue their own load; the second is an unconditional overwrite (the same lazy-degradation cost already accepted above). Cross-alias collapse applies only after the first store has indexed the aliases; before that point, each unindexed path is independently cold. This lazy
degradation is the accepted pragmatic cost of not suspending the provider.

### 6.2 Stability drives revalidation and the deadline

The three-level `Stability` (`file_attrs.rs`) governs both, per leaf:

- **Immutable**: no deadline; served until capacity/invalidation. A new version is
  a new identity/path (e.g. `versions/{v}/paper.pdf`). Never revalidates.
- **Mutable**: deadline = write-time + a short TTL (host config default, a few
  seconds, overridable per mount). Past it a read revalidates per §6.1.
- **Volatile**: deadline = write-time (TTL 0); ranged re-read every time (requires
  a Ranged source, `file-attributes.md (Legal combinations)`).

**Freshness is per-leaf atomic.** A leaf's lookup, attrs, dirent, and file-content
records carry one shared deadline and one generation, written together at
materialization and refreshed together. The host never serves a leaf whose records
disagree on freshness, so `stat` and `read` of one leaf can never observe two
versions. Stability is declared per leaf in the block (static, known before load),
defaulting to the object's declared Stability. The object's declared Stability is `#[object(stability = ...)]`, `Mutable` when omitted (§11).

Revalidation is **deduped by `ObjectId`**, not by FUSE operation: one canonical has
one validator, so a single conditional GET refreshes every leaf. The host coalesces
revalidation of a given `ObjectId` within a short window spanning multiple reads, so
`cat title state user` or `grep -r` over one object issues at most one conditional
GET. Concurrency rules are in §6.4.

### 6.3 Preload

A `Objects` collection preloads each row's **eager** field leaves into the view
cache via `fs-write` effects (§7.1), using the same `Issue::title` used on cold
reads. Preload never writes the object cache (the row is not the raw single-item
body). Each preloaded leaf carries the object's `LogicalId` (so it joins the
reverse index, §7.2) and is fenced (§7.3).

Preload leaves carry no validator and play **no role in freshness ordering**. There
is no list-token-versus-ETag comparison: **overwrite eviction is unconditional**
(§7.2), so whenever a single-item load later stores a canonical for that
`LogicalId`, the rendered bytes of every prior indexed leaf for it — including preload-only leaves — are dropped first (the path↔id entries are kept; only the rendered bytes are re-derived). A preloaded Mutable leaf serves until its deadline, then re-derives
through the object-miss branch of §6.1 (first re-derive is a full single-item load
that obtains the real validator; later revalidations are cheap).

**Canonical beats preload.** A preload `fs-write` for an `ObjectId` that already has
a stored canonical is dropped: the canonical is authoritative and the leaf renders
from it on demand. So a stale list row arriving after a single-item load cannot
overwrite a fresher canonical-derived leaf; preload only fills leaves for objects
the cache has not yet loaded.

### 6.4 Concurrency: singleflight and the loser path

Revalidation/load for one `ObjectId` is **singleflight**: concurrent reads of any
of its leaves attach to one in-flight load. The rules:

- The in-flight load captures its read generation at start. On completion its
  `canonical-store`/`fs-write` are fenced (§7.3): admitted only if no tombstone for
  the `ObjectId` is newer than that generation.
- If an invalidation lands while the load is in flight, the load **loses**: its
  writes are rejected, and every attached waiter (including the originator)
  discards the result and re-loads at the new generation rather than rendering the
  rejected bytes or resetting any deadline.
- A winning load installs the canonical (overwrite eviction first), materializes
  the requested leaf, sets the shared deadline, and wakes waiters, which render
  from the freshly stored canonical.

This makes "a 200 overwrite racing an `invalidate object`" deterministic: the
invalidation's tombstone fences the overwrite, so the stale 200 never lands and the
next read re-fetches.

**Volatile leaves bypass singleflight.** Singleflight and `ObjectId`-keyed dedup
apply only to Mutable canonical revalidation. A Volatile leaf is a ranged/streamed
source (the blob path, `file-attributes.md (Legal combinations)`), not an object-canonical render, so
concurrent reads (two clients `tail -f`-ing one leaf) each issue their own ranged
read and never serialize behind a single in-flight load, and distinct observations
are never collapsed into one snapshot.

### 6.5 Persistence across restart and remount

| Store | Durability | On restart |
|---|---|---|
| Object cache (`object` fjall keyspace): canonical bytes + validator + the object's `id → {alias paths}` set | durable | retained; Mutable revalidates on next read, Immutable trusted |
| Path↔id index | durable (stored with the object cache) | retained, so a previously-seen path finds its canonical without a blind refetch |
| View cache (`view` fjall keyspace): rendered leaf bytes + dirents | ephemeral | deleted and recreated empty |
| Tombstones | in-memory | cleared |
| Per-mount generation | in-memory, monotonic within a run | reset to 0 |

The split that matters: the **path↔id index and the alias set are durable** (they
live with the object record), so after restart a read of a previously-seen path
still resolves to its `ObjectId` and reuses the durable canonical — Mutable
revalidates cheaply, Immutable is trusted, neither refetches blind. This is what
makes the durable object cache actually useful. Only the **rendered view bytes** are
ephemeral; they recompute from the durable canonical on first read. The generation
reset to 0 is safe because the only thing it fences — rendered view bytes and
in-flight loads — does not survive restart, and no operation spans the boundary. A
lost tombstone (an invalidation just before a crash) is self-correcting: the
durable canonical it would have evicted is Mutable (the next read revalidates) or
Immutable (never invalidated, a new version is a new id). The host allocates the
generation per mount; each invalidation and each load captures the current value
for the fence (§7.3).

## 7. Effects, the reverse index, and invalidation

### 7.1 Effects and the wire change

The provider returns effects carrying its `LogicalId`; the host scopes them by
mount. The WIT changes from `object-cache-primary.md` are:

```
canonical-store { id: logical-id, validator, bytes, view-leaves: list<path> }
fs-write        { id: option<logical-id>, path, attrs, source, content-type, exhaustive }   // gains optional id
invalidate      { object(id) | listing(path-or-prefix) }                                    // intent-tagged, id-bearing
not-found       { id: option<logical-id> }                                                  // non-error, fenceable NotFound terminal (lookup + read)
```

- Note: `canonical-store` and `canonical-input` rename the wire field `version` → `validator` relative to `object-cache-primary.md`'s WIT (the underlying token is the same `VersionToken`).
- `canonical-store` is **multikey**: it keys the canonical by `LogicalId` and lists
  full `view-leaves` paths for **the current request's attach prefix expanded over
  any finite `Facet`'s `choices()`**. For `StateFilter = {open, all}` one store from
  a read of `open/42` lists both `open/42/*` and `all/42/*`. This expansion is
  SDK-internal path construction (the SDK renders the paths it is materializing from
  the matched route + leaves + Facet choices); it is **not** the removed *external*
  `build_path`, and there is **no privileged "home" attach**. Unbounded aliases (a
  discarded prefix capture such as arXiv `category`) cannot be enumerated and are
  indexed lazily on first visit through that prefix. Canonical bytes are stored once
  under the id; `view-leaves` are full paths, not anchor-relative remainders.
- `lookup_child` / `list_children` results carry an optional `LogicalId` per entry
  that names an object anchor. The host indexes that entry's inode/dirent under the
  id, so `invalidate object(id)` evicts the object's own name entry and a later
  `lookup` re-dispatches (ENOENT after deletion). Entries that are not object
  anchors carry no id.
- `fs-write` **extends** today's record (path, attrs, byte source, content-type,
  directory exhaustiveness) with an **optional** `id`: object leaves carry it (so
  they join the reverse index and are fenced); structural/non-object materializations
  (DNS, plain directories) leave it `none`. Nothing in the existing materializer
  payload is dropped.
- `invalidate` is **intent-tagged and id-bearing**: `object(id)` names the object
  directly (the provider already computed it — the host never resolves a path), and
  `listing(path-or-prefix)` names a directory subtree. The legacy `invalidate-path`
  / `invalidate-prefix` map to `listing`.
- `not-found` is the `Load::NotFound` outcome as a **non-error**, id-bearing terminal (not an `op-result::error`, which carries no effects). It appears as `lookup-child-result::not-found(option<logical-id>)` and as a read-path NotFound terminal carrying `option<logical-id>`. The host stores a fenced negative record (§7.5) keyed by that `ObjectId`; absence is cacheable (§2.1).

### 7.2 One mount-scoped index, learned from effects

The host maintains a single bidirectional index, mount-scoped, populated only by
`canonical-store` and id-bearing `fs-write`:

```
path  -> ObjectId         (forward: used on a read to find the canonical to push)
ObjectId -> { paths }     (reverse: used by eviction/invalidation)
```

It is **learned, never computed**: the host does not parse paths (§3.2). The forward
direction exists for a path only after that path was materialized by an effect; a
path with no entry is a cold dispatch (§6.1). The index is canonical-independent — a
preload-only object with no canonical bytes still has index entries.

The index is the single source of truth for eviction:

- **Overwrite (a 200 / new canonical)**: replaces the canonical bytes + validator,
  **drops the rendered view bytes** for the id's known paths (forcing re-render from
  the new canonical), and **unions** the new store's alias paths into the id's path
  set. It does **not** remove path↔id entries, so previously-indexed aliases
  (finite-Facet or lazily-added, e.g. an arXiv `category` path) survive an overwrite
  and re-render on next read rather than cold-refetching. Dropping bytes (not the
  index) is what gives version coherence while preserving the alias set.
- **`invalidate object(id)`** is the *only* operation that removes an id's path↔id
  entries — full eviction of canonical, validator, rendered bytes, alias set, and
  negative record. A later read of any of those paths is a cold dispatch that
  re-establishes the entry.
- **Capacity eviction of a canonical** drops its validator and rendered bytes
  atomically (so §6.1's object-miss branch always means "no canonical and no
  validator," never a stranded validator); it may retain the path↔id entries, so a
  re-read finds the id, misses the canonical, and does a full `load(None)`.

**One path, one id.** Each path resolves to exactly one route, hence one
`LogicalId`; two registrations that claim the same path (an object child file vs a
nested object bound at the same path) are a registration-time error caught in
`start()`. The forward map therefore never holds a path under two ids, and a
multikey `canonical-store` (§7.1) only ever adds alias paths that all resolve to
the storing object. At materialization the host **also rejects at runtime** any
effect (`canonical-store`, `fs-write`, or a lookup/list entry id) that maps an
already-indexed path to a *different* `ObjectId` — a provider bug — so the forward
map stays single-valued in operation, not only at registration.

**Completeness invariant**: the host caches nothing it was not taught an `id` for.
Every object view leaf arrives via an effect carrying its `LogicalId`. Therefore
`invalidate object(id)` reaches every cached leaf of that object, even ones
materialized through a different view prefix, and a not-yet-materialized path is
simply absent (nothing to strand; the next read is a cold dispatch).

### 7.3 The fence covers every object-derived cache write

A per-mount generation plus a per-`ObjectId` tombstone rejects any write derived
from data read before a concurrent invalidation. The fence covers **all** writes
that depend on read data: `canonical-store`, id-bearing `fs-write` (preload),
rendered view leaves, **and negative records (§7.5)**. Each carries its read
generation; a write is admitted only if no tombstone for its `ObjectId` is newer.
Id-less structural `fs-write`s carry no id and are out of fence scope by construction.
A list response, a preload, or a NotFound racing an invalidation can no longer
reinstall stale state. The loser-path semantics are in §6.4.

### 7.4 Invalidation distinguishes object from membership

- **`invalidate object(id)`**: evict the canonical, its validator, and every indexed
  view leaf for the `ObjectId` (all prefixes, dropping cross-prefix siblings like
  open/42 ↔ all/42) and any negative record. The provider supplies the id (it
  computed it from the event), so the host does no path resolution. Emitted for a
  changed issue, a webhook, or a poller noticing an object edit.
- **`invalidate listing(path-or-prefix)`**: evict only the dirent/listing entries
  under the path/prefix; do **not** touch member objects' canonicals. Emitted when a
  collection's membership changes (arXiv category, a docker container set).

The SDK exposes `cx.invalidate_object(&key)` (which calls `key.anchor()`) and
`cx.invalidate_listing(path_or_prefix)`. The fence applies to both.

For a **file-shape object** (§9) the object *is* its parent dirent, so
`invalidate object(id)` evicts that id-indexed name entry directly (the entry was
indexed via the `lookup_child`/`list_children` id, §7.1), and `cat` of a deleted
file-object ENOENTs on the next access. The parent listing's enumeration may still
show the name until its own `invalidate listing` or readdir expiry — the standard
"lookup is authoritative, readdir may be non-exhaustive" tolerance — so a deletion
event should emit both `invalidate object(id)` and `invalidate listing(parent)` for
prompt `ls` freshness.

### 7.5 Negative (NotFound) records

`Load::NotFound` produces a negative record keyed by `ObjectId`: `{ absent, as-of
generation, deadline }`, **fenced** like any other write (§7.3). On the wire this rides the non-error, id-bearing `not-found` terminal (§7.1), so the negative carries the `LogicalId` and is fenced like any other write; it is never an `op-result::error`. It carries a short
deadline (Mutable-like) so a not-yet-created issue number or paper does not become
sticky ENOENT, and it is cleared by any `invalidate object(id)` for that id. A read
hitting a live, unfenced negative record returns ENOENT without dispatch; past the
deadline it re-loads. Because it is fenced, a NotFound that raced an object-creating
invalidation is rejected and the next read re-loads.

## 8. Representations and rendering

Content-type markers (`Json`, `Markdown`, `Atom`, `Octet`, `Yaml`, `Html`, `Diff`,
…) are zero-size types implementing `trait Format { const CT: ContentType; }`, a
thin type-level layer over the existing `ContentType` enum (not a replacement). A
provider may add a marker for a custom CT by implementing `Format`.

Every object exposes its **canonical (source) leaf automatically**: one leaf named
`<stem>.<ext>` serving the stored canonical bytes verbatim, where
`ext = canonical_ct.extension().unwrap_or("raw")` (`content_type.rs`). A
JSON canonical yields `item.json`; an Atom canonical yields `paper.atom`, the raw
upstream body, inspectable and byte-equal to the single-item GET.
The author never declares it.

`o.representations(stem, renders)` declares only the **derived renders**, as a tuple
of `Format` markers; the source leaf is implicit:

```rust
o.representations("item", (Markdown,))?;   // item.json (source, canonical=Json) + item.md (render)
o.representations("paper", ())?;           // paper.atom (source, canonical=Atom)
o.file("paper.json").project(Paper::paper_json_leaf)?; // projected JSON metadata
o.representations("repo", ())?;            // repo.json (source) only, no renders
```

The tuple's bound is unconditional — `Self: Representable<F>` for every `F` in it —
because the source leaf is not in the tuple, so there is no "require an impl except
for the canonical" conditional to express. A declared render with no impl is a
compile error.

**The runtime bridge.** Reads arrive as `(path, ContentType)`. The route holds a
representation table: the canonical CT maps to a *verbatim* entry (return the stored
bytes unparsed), and each render `F` in the tuple maps to an erased closure
`fn(&[u8]) -> Result<Vec<u8>>` = `|canon| O::parse_canonical(canon)?.represent::<F>()`,
keyed by `F::CT`. At read time the SDK maps the leaf's extension to a `ContentType`,
looks it up, and serves verbatim or invokes the closure. A render whose CT equals
the canonical CT, or two leaves with the same extension, is a registration-time
error. This is the concrete `ContentType → Representable<F>` dispatch the type-level
bounds alone do not provide.

## 9. Anchor shapes

Both shapes are kept. **Dir-shape** (`r.object`): the anchor is a directory holding
`<stem>.<ext>` representations plus field leaves and children. **File-shape**
(`r.file_object`): the anchor is a multi-extension file `<name>.<ext>` with
representations only; the File-shape builder has no `file`/`dir`/`field` methods, so
a field/child is a compile error.

## 10. Non-object providers (DNS)

DNS stays route-shaped: it never calls `r.object`, emits no `canonical-store`, and
self-selects out of the object cache (volatile, no canonical documents). Its
captured handlers are key methods (`DomainRecordKey::read`, `ResolverReverseKey::read`);
capture-less ones stay free functions (`root_list`, `resolvers_file`). Any
structural `fs-write` it emits leaves `id` `none` (§7.1). The default-resolver path
normalizes its absent resolver to the configured default inside
`ResolverName::from_str`, the only non-structural identity case, kept in the capture
type (consistent with §3 normalization).

## 11. Macro responsibilities

- `#[object(kind = "...", key = KeyType, canonical = ..., parse = parse_fn, stability = <Immutable|Mutable|Volatile>)]` on the data struct: generates the `Object` impl metadata (`type Key`, kind, canonical CT, default stability, and optional custom canonical parser). `canonical` defaults to `Json`, `stability` defaults to `Mutable`, and `parse` is omitted for serde-JSON canonical objects. Fields are hand-written methods; renders are hand-written `Representable<F>` impls. No `#[omnifs(field(...))]` attributes remain.
- `#[path_captures]` on the key struct: generates `FromCaptures` (Facet-aware),
  `anchor()` (`LogicalId` from non-`Facet` fields + `Object::kind()`, using each
  field's normalized `Display`), `#[flatten]` parent embedding, and a generated
  `#[test]` that asserts each identity field round-trips `FromStr`/`Display`
  idempotently for a sampled set (the contract from §3; not a compile-time proof).
- `#[provider(...)]`: unchanged entrypoint; `start` returns `Result<State>`.

## 12. Worked example: GitHub `start()`

```rust
fn start(_config: Config, r: &mut Router<State>) -> Result<State> {
    r.dir("/{owner}").handler(OwnerKey::repos)?;                       // dynamic collection
    r.object::<Repo>("/{owner}/{repo}", |o| { o.representations("repo", ())?; Ok(()) })?; // gate = load

    r.dir("/{owner}/{repo}/issues").dirs::<StateFilter>()?;            // open/all
    r.dir("/{owner}/{repo}/issues/{filter}").handler(IssueListKey::list)?;
    r.object::<Issue>("/{owner}/{repo}/issues/{filter}/{number}", |o| {
        o.representations("item", (Markdown,))?;
        o.file("title").project(Issue::title)?;
        o.file("body").project(Issue::body).lazy()?;                  // truncated in lists -> lazy
        o.file("state").project(Issue::state)?;
        o.file("user").project(Issue::user)?;
        o.dir("comments").handler(IssueKey::comments)?;
        o.file("comments/{idx}").handler(IssueCommentKey::read)?;
        Ok(())
    })?;

    r.dir("/{owner}/{repo}/pulls").dirs::<StateFilter>()?;
    r.dir("/{owner}/{repo}/pulls/{filter}").handler(PullListKey::list)?;
    r.object::<PullRequest>("/{owner}/{repo}/pulls/{filter}/{number}", |o| {
        o.representations("item", (Markdown,))?;
        o.file("title").project(PullRequest::title)?;
        o.file("body").project(PullRequest::body).lazy()?;
        o.file("state").project(PullRequest::state)?;
        o.file("user").project(PullRequest::user)?;
        o.dir("comments").handler(PullCommentKey::dir)?;             // issues endpoint upstream, pulls path
        o.file("comments/{idx}").handler(PullCommentKey::read)?;
        o.file("diff").handler(PullKey::diff)?;                       // pulls-only, structural (no .when)
        Ok(())
    })?;

    r.treeref("/{owner}/{repo}/repo").handler(RepoKey::tree)?;

    r.dir("/{owner}/{repo}/actions/runs").handler(RunListKey::list)?;
    r.object::<Run>("/{owner}/{repo}/actions/runs/{run_id}", |o| {
        o.representations("run", ())?;
        o.file("status").project(Run::status)?;
        o.file("conclusion").project(Run::conclusion)?;
        o.file("log").handler(RunKey::log)?;                          // blob via blob cache
        Ok(())
    })?;
    Ok(State::default())
}
```

### 12.1 Issues and pulls are enforced-disjoint objects

`Issue` and `PullRequest` are separate self-contained objects bound at literal
`/issues/` and `/pulls/`, each with its own duplicated block. Disjointness is
**enforced in `load`**: `IssueKey::load` returns `NotFound` for a number whose
fetched item is a pull request (`pull_request` set), and `PullKey::load` 404s for
non-PR numbers via the pulls endpoint. So a number resolves to exactly one of
`github.issue|…|n` or `github.pull|…|n`, never both: no shared upstream object, no
linked identity, no cross-family invalidation. A PR's comments are *fetched* through
the issues endpoint inside `PullKey`'s load but are *projected and invalidated* at
`/pulls/n/comments`. The kind disambiguation an event needs (a webhook payload's
`pull_request` flag) is provider-internal logic in the event handler, the same place
the listing filter applies; it emits `invalidate object(id)` for the resolved kind.
The pulls-only `diff` is structural (absent from the `Issue` block). The shared
field block (~6 lines) is duplicated, accepted as the cost of full separation.

## 13. Worked example: arXiv

```rust
fn start(_config: Config, r: &mut Router<()>) -> Result<()> {
    let papers = object::<Paper>("/{paper}", |o| {
        o.representations("paper", ())?;                               // paper.atom source
        o.file("paper.json").project(Paper::paper_json_leaf)?;         // projected JSON metadata
        o.dir("versions").project(Paper::versions)?;                  // warm: v1..vN from latest_version
        o.file("versions/{version}/paper.json").project(Paper::version_json)?; // warm: re-project at {version}
        o.file("versions/{version}/paper.pdf").immutable().handler(VersionKey::pdf)?;       // blob
        o.file("versions/{version}/source.tar.gz").immutable().handler(VersionKey::source)?;// blob
        o.file("paper.pdf").handler(PaperKey::pdf)?;
        o.file("source.tar.gz").handler(PaperKey::source)?;
        Ok(())
    });
    r.attach("/papers", &papers)?;
    r.attach("/categories/{category}/papers", &papers)?;              // category: pure-nav prefix capture

    r.dir("/categories/{category}").handler(CategoryKey::sub)?;
    r.dir("/categories/{category}/papers").handler(CategoryKey::recent)?; // ObjectListing::Names (ids only)
    Ok(())
}
```

`versions` and per-version metadata are warm projections of the one loaded Atom
canonical (no refetch). `category` is a pure-navigation prefix capture absent from
`PaperKey` (§4.5), so both attaches share `arxiv.paper|paper=…`. A category
membership change emits `invalidate listing` for `/categories/{category}/papers`,
which never evicts a paper canonical (§7.4).

## 14. Invariants and tests

1. **Identity collapse**: read `issues/open/42/title` then `issues/all/42/title` →
   one upstream load, one object-cache entry.
2. **Mount isolation**: the same `owner/repo/number` under two mounts never shares a
   canonical; `issues/42` vs `pulls/42` never share.
3. **Canonical is raw**: `cat issues/all/42/item.json` byte-equals the single-item
   GET, not a re-serialized list row.
4. **Single-item-equivalence**: an eager field's cold-read bytes equal its preloaded
   bytes; a `.lazy()` field is never preloaded.
5. **Preload coherence**: a preload-only leaf's rendered bytes are dropped (re-derived) when a newer canonical is stored (unconditional overwrite); the path↔id entry is kept and only the rendered bytes change. `cat title` never returns a value older than a just-read `item.json`. (To evict the entry entirely, use `invalidate object`.)
6. **Object-miss revalidation**: a Mutable leaf read after its deadline with no
   canonical does a full load, never a 304, and never sends a list token as a
   validator.
7. **No stranded 304**: evicting a canonical evicts its validator; no conditional
   request is issued without canonical bytes.
8. **Fence covers preload and negatives**: a list response, a preload, or a NotFound
   racing an `invalidate object` does not land.
9. **Singleflight loser**: a load fenced by a mid-flight invalidation is discarded by
   all waiters, which re-load; no waiter renders the rejected bytes or resets a
   deadline.
10. **Atomic leaf freshness**: `stat` and `read` of one leaf never observe two
    versions.
11. **Revalidation dedup**: `cat title state user` over one expired object issues at
    most one conditional GET.
12. **Cold dispatch only**: the host never resolves a path it has no index entry for;
    such a read dispatches to the provider, which supplies the id.
13. **Structural gating**: `diff` resolves under `pulls`, ENOENTs under `issues`; an
    Issue load for a PR number returns NotFound.
14. **Invalidation intent**: `invalidate object` evicts canonical + all view leaves
    across prefixes; `invalidate listing` leaves member canonicals intact.
15. **Representation dispatch**: a read for a declared render CT invokes its erased
    fn; an undeclared CT is ENOENT; duplicate-CT registration fails at build.
16. **Negative records**: a NotFound is cached and returns ENOENT without dispatch
    until its deadline or an `invalidate object`.
17. **Attach symmetry**: `/papers/X` and `/categories/c/papers/X` resolve to one
    identity and one cache entry.
18. **Route-map visibility**: registered routes include `diff`/`comments`.

## 15. Migration

Big-bang migration: the SDK surface (`Object`/`Key`/`Facet`/`Representable`/
`Format` markers, the representation builder + erased render table, `r.object`/
`file_object`/`attach`/`dirs`/collections, `#[flatten]`), the effects/WIT protocol
(provider-emitted `LogicalId`, `canonical-store` + id-bearing extended `fs-write`,
intent-tagged id-bearing invalidation, fenced preload + negatives), the host caches
(object durable with validator; view with atomic per-leaf deadline; learned
bidirectional index; negative records; singleflight revalidation deduped by
`ObjectId`), and all six providers (github split into enforced-disjoint
Issue/PullRequest, arxiv, db, linear, docker, dns) migrate together. The old
`bind`/`#[object]`-field/route-shaped surface is deleted in the same change; no dual
model ships.

## 16. Decision log

| # | Decision |
|---|----------|
| 1 | Route-context captures use a `Facet<T>` wrapper; identity = unwrapped fields. |
| 2 | Keep both Dir and File anchor shapes. |
| 3 | Two constructors: `r.object` (Dir, stem in `representations`) / `r.file_object` (File). |
| 4 | Canonical CT on `#[object(canonical=…)]`; `Representable<F>` per render; markers are ZSTs over `ContentType`; verbatim leaves carry no bound. |
| 5 | `load` on `Key`, receives cached validator; `Load = Fresh { value, canonical } \| Unchanged \| NotFound`. |
| 6 | Revalidation maps to Stability: Immutable warm / Mutable revalidate / Volatile refetch. |
| 7 | Stability per-leaf, static, object default; revalidation singleflight, deduped by `ObjectId`. |
| 8 | Preload writes eager fields as view leaves; lazy fields + reps on demand; never writes canonical. |
| 9 | View hit serves warm; the view cache gains an atomic per-leaf deadline. |
| 10 | Deadline is Stability-derived (Immutable ∞ / Mutable short / Volatile 0); object cache deadline-less. |
| 11 | Effects carry the provider's `LogicalId`; invalidation is intent-tagged and id-bearing. |
| 12 | GitHub issues and pulls are two enforced-disjoint objects at literal route families. |
| 13 | Two object types, blocks duplicated; SDK keeps 1:1 key↔object. |
| 14 | Collections return `(O::Key, O)` pairs; eager fields must be single-item-equivalent. |
| 15 | Multi-mount via detached route + `attach`; only pure-nav prefix captures may differ. |
| 16 | Identity is two-layer: provider `LogicalId` (kind + normalized captures), host `ObjectId` (mount ++ LogicalId). |
| 17 | Captured handlers are `&self` key methods (incl. DNS); capture-less are free fns. |
| 18 | `Object` names the data; the key trait is `Key`. |
| 19 | Drop `R`/`Routes`/`build_path`; `start` returns `State`; detached handles are local. |
| 20 | Migration is big-bang; no dual model. |
| 21 | The host is byte-level and path-keyed; the provider owns all path→object/id mapping; the host learns path↔id only from effects and never parses paths (§3.2). |
| 22 | The host never suspends the provider mid-read (hard invariant); identity collapse uses a multikey `canonical-store` that enumerates finite-Facet aliases from `choices()`; unbounded aliases index lazily (§3.2, §7.1). |
| 23 | Path↔id index + alias set are durable (with the object cache); only rendered view bytes are ephemeral; overwrite drops bytes not aliases; `invalidate object` is the only index remover; canonical beats preload; runtime path→different-id rejection (§6.3, §6.5, §7.1, §7.2). |
| 24 | The canonical/source leaf is automatic (`<stem>.<ext>` from `canonical_ct.extension().unwrap_or("raw")`); `o.representations(stem, renders)` declares only derived renders as a tuple; `.identity()`/`.octet()` dropped (§8). |

### 16.1 Adversarial-review resolutions

Round 1 (shape), round 2 (boundary/wire/concurrency), round 3 (the alias /
resolution contract), and round 4 (the durable index), all folded into the body:

| Issue | Resolution |
|-------|------------|
| Mount/provider scope in identity | Two-layer id: provider `LogicalId`, host `ObjectId = mount ++ LogicalId` (§3). |
| Identity normalization | Normalized `Display`, normalize in `FromStr`; round-trip enforced by a generated test, not a macro proof (§3, §11). |
| Preload-only leaf survives newer canonical | Unconditional overwrite eviction over the canonical-independent reverse index (§6.3, §7.2). |
| Un-materialized path invalidation | Provider emits `invalidate object(id)`; host never parses paths; completeness invariant (§3.2, §7.2, §7.4). |
| Host computes id across the boundary | It doesn't: provider supplies `LogicalId`; host learns path↔id from effects, dispatches cold misses (§3.2, §6.1). |
| `Load::Fresh` lost canonical bytes | Restored: `Fresh { value, canonical: { bytes, validator } }`, provider-supplied (§2.1). |
| `fs-write` too lossy / no id | Extended (not replaced) with optional `id`; keeps attrs/source/CT/exhaustiveness (§7.1). |
| List-token vs ETag ordering | Removed from freshness; overwrite eviction is unconditional; token has no role (§6.3). |
| 304 after canonical eviction | Validator evicted with canonical; object-miss ⇒ `since=None` ⇒ full fetch (§6.1, §7.2). |
| View TTL underspecified / "no TTLs" | Concrete atomic per-leaf deadline checked on read; object cache deadline-less (§6.2). |
| Per-record deadlines tear a leaf | Freshness is per-leaf atomic across lookup/attrs/dirent/content (§6.2). |
| Dedup misses multi-file reads | Deduped by `ObjectId` across a window; singleflight (§6.2, §6.4). |
| Dedup/invalidation loser path | Specified: fenced load loses, waiters re-load, no deadline reset (§6.4). |
| Fence excludes fs/negatives | Fence covers canonical-store, fs-write, rendered leaves, and negative records (§7.3, §7.5). |
| `Representable<F>` runtime bridge | Erased render-fn table keyed by `ContentType`; verbatim vs render entries; dup-CT rejected (§8). |
| Negative records unfenced / sticky ENOENT | Fenced negative records with a short deadline, cleared by `invalidate object` (§7.5). |
| Intent-tagged invalidation contract | Exact WIT variants `object(id)` / `listing(path-or-prefix)` (§7.1, §7.4). |
| Children needing dropped context | Read via flattened key (carries Facets); needed context must be a key Facet (§4.4). |
| Single-item-equivalence uncheckable | Documented provider invariant + sampling test #4; over-claim removed (§4.3, §14). |
| Issue/PR shared invalidation | Rejected: enforced-disjoint `load`; a number is exactly one identity (§12.1). |
| Cross-prefix cold read refetches (alias collapse) | Multikey `canonical-store` enumerates finite-Facet aliases; sibling reads hit the shared canonical; no provider suspend (§3.2, §6.1, §7.1). |
| Overwrite eviction strands aliases | Re-store is multikey, so finite aliases are reinstalled; lazily-added aliases re-resolve cheaply from the in-hand canonical (§6.3, §7.2). |
| Lookup/list create id-less inodes/dirents | `lookup_child`/`list_children` entries carry an optional `LogicalId`; object name entries are indexed and evicted by `invalidate object` (§7.1). |
| Path→id conflict unhandled | One path resolves to one route → one id; overlapping registrations are a `start()`-time error (§7.2). |
| Volatile + singleflight serializes streams | Volatile/ranged leaves bypass singleflight; only Mutable canonical revalidation dedups (§6.4). |
| Generation/persistence across remount | Explicit persistence table; generation reset safe because rendered view bytes are ephemeral and no op spans restart (the path↔id index is durable, §6.5); lost tombstones self-correct via Mutable revalidation. |
| File-shape parent-dirent coherence | Object invalidation evicts the id-indexed name entry; readdir lag is standard tolerance; deletion emits object + listing invalidation (§7.4). |
| Preload serves field bytes during window | Invariant #3 scoped to representations (never preloaded); field freshness bounded by deadline + equivalence assertion (§4.3). |
| Durable object cache unreachable after restart | Path↔id index + alias set are durable; only rendered bytes ephemeral; a known path reuses its canonical (§6.5). |
| Lazy aliases stranded by overwrite | Overwrite drops rendered bytes but keeps + unions the alias set; only `invalidate object` removes it (§7.2). |
| Multikey = hidden reverse path synthesis | Named as SDK-internal path expansion, distinct from external `build_path`; no privileged home (§7.1). |
| Preload overwrites canonical-derived leaves | Canonical beats preload: a preload `fs-write` is dropped when a canonical exists for the id (§6.3). |
| One-path-one-id needs runtime enforcement | Host rejects any effect mapping an indexed path to a different id, not only at registration (§7.2). |
| Representation builder over-chained | Source leaf automatic; `o.representations(stem, renders)` tuple; `.identity()`/`.octet()` removed (§8). |
