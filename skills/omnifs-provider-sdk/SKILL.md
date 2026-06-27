---
name: omnifs-provider-sdk
description: Build omnifs providers (sandboxed WASM components that project external services into filesystem paths) with the omnifs-sdk router and object model. Use when writing or modifying a provider under providers/, choosing between object-oriented and path-oriented routes, designing a path schema, wiring auth/manifest/capabilities, or debugging provider dispatch, caching, or staleness behavior.
---

# Building omnifs providers

A provider is one `wasm32-wasip2` component implementing the `omnifs:provider` WIT contract through `omnifs-sdk`. The host mounts it, calls browse operations (lookup-child, list-children, read-file, open/read-chunk/close), and runs every side effect (HTTP, git, blob, archive) for it via suspend/resume callouts. The provider owns meaning (what paths exist, what bytes they hold); the host owns trust, I/O, and all caching.

Read the rustdocs in `crates/omnifs-sdk/src/lib.rs` for the full guide; `providers/DESIGN.md` for the flavour doctrine with per-provider rationale; this skill is the operational distillation.

## The golden rules

1. **The host owns caching.** Never add provider-side caches, LRUs, or TTLs. Freshness comes from invalidation effects and version tokens, not timers in your code.
2. **Preload discipline.** If a payload in hand already contains fields or children the user can read next, emit them now (eager leaf projections, listing entries with attrs). If the list payload is not the full leaf contract, emit a deferred file with honest attrs instead of pretending it is.
3. **`lookup` is the authoritative name oracle; `readdir` may be non-exhaustive.** A listing's `exhaustive` flag means "these are all the names I know"; lookup may still resolve names absent from the latest listing.
4. **Never write stub handlers for intermediate directories.** Any registered route's literal prefix is auto-navigable; listings merge your enumeration with literal sibling routes at that depth.
5. **Capture parse rejection = fallthrough**, not NotFound: the route becomes a non-candidate and dispatch tries the next-most-specific route. Use it (e.g. `@latest` vs `v{n}` selectors as distinct types).
6. **Captures validate syntax only.** Existence is decided by handlers, lookup intent, or `Object::load`. Exception: a finite `choices()` set when the universe is truly static or a dynamic route would otherwise synthesize false anchors (the DB provider's table-name case).
7. **Attribute honesty.** Inline bytes require `Size::Exact(len)` and are capped at 64 KiB (`MAX_PROJECTED_BYTES`); bigger or unknown content is deferred. `Stability::Live` requires `Deferred { read: Ranged }` (the validator rejects anything else). Dynamic upstream = dynamic projection; versioned upstream = stable projection.
8. **Identity vs facets.** A key's non-`Facet` fields, in declaration order, ARE the object's identity (its logical id and cache key). Route-context captures that must not split the cache (list filter, version selector, team alias) are `Facet<T>`. Changing an object's `kind` string or identity captures orphans every cached object.
9. **Auth never appears in provider code.** Credentials are declared via the `auth = ..` argument to `#[omnifs_sdk::provider]` and materialized into requests by the host. `#[endpoint(auth = ..)]` is rejected at compile time by design.
10. **Reuse the SDK, stdlib, and imported crates before writing plumbing.** Check `omnifs-sdk` projections, endpoint helpers, object/load APIs, typed captures, `time`, `url`, `serde_json`, `strum`, and other already-present workspace dependencies before adding local parsers, date math, enum tables, HTTP glue, cache shims, or projection helpers.
11. **Use `strum` for finite enum path segments.** Derive `EnumString`, `AsRefStr`, `Display`, and usually `VariantArray` with `serialize_all = "snake_case"` instead of hand-writing parse/display ladders or parallel `&[&str]` tables. Put the single universe on the enum (`VariantArray::VARIANTS` or an enum-owned `ALL`), and derive filenames, entries, and `PathSegment::choices` from that one list.
12. **Use `hashbrown::HashMap`** (re-exported by the SDK) for provider-internal maps.
13. **Error semantics:** `NotFound` for absent upstream resources or rows; `InvalidInput` for impossible path syntax; `rate_limited(..).with_retry_after(..)` on 429 (drives the SDK breaker and host window); `from_http_status` for the rest. Put operation context in the message.
14. **No provider unit tests in-crate.** Verify behavior through host-driven integration tests and the live `omnifs dev` container (repo policy).

## One route API: which face

There is one route API. `file` and `dir` are the nouns; the *face* you put under them is the choice. Decide per route family, not per provider; hybrids are normal (arxiv, github). Object faces are the default for anything with identity and replayable canonical bytes; raw `r.dir`/`r.file`/`r.treeref` handlers are the escape hatch.

| Question about the path family | Answer | Use |
|---|---|---|
| Is there ONE canonical upstream payload (an issue, a paper, a PR) serving several derived leaves (`title`, `body`, `item.json`)? | yes | **Object face**: `r.object::<O>(t, \|o\| ..)` with `#[omnifs_sdk::object]` + a `#[path_captures]` key + an inherent `async fn load`; declare `o.file(..).canonical/representation/derive` leaves |
| Is the object a single cacheable file (one canonical/render/blob), not a directory of leaves (Oura `{day}/{collection}`)? | yes | **File-shaped object**: `r.file_object::<O>(t, \|o\| { o.canonical::<Json>()?; .. })` |
| Is the data a query result or operational state with no durable object behind it (DNS answer, `docker ps`, a DB row read, live metrics)? | yes | **Direct face** under an object (`o.file(..).direct(method)`), or a raw `r.dir`/`r.file` handler; emit NO canonical-store effects |
| Are the bytes large or host-cacheable, and should never cross the WIT boundary (a PR diff, a PDF)? | yes | **Blob face** `o.file(..).blob(method)` returning `BlobFile<F>` |
| Is the leaf ranged or `Live` (pod logs, `tail -f`)? | yes | **Stream face** `o.file(..).stream(method)`; the only face that may be `Live` |
| Does a directory list child objects (repos under an owner, issues, comments)? | yes | **Collection face** `o.dir(name).collection::<C>(method)` returning `Collection<C, Cur>`; `C` must be its own `r.object::<C>` |
| Is the subtree a real tree the host can materialize wholesale (git repo, release archive)? | yes | **Tree face** `o.dir(name).tree(method)` returning `TreeRef`, or raw `r.treeref(t).handler(..)` |
| Is the same object reachable at a second path (by-name and by-id)? | yes | `let h = r.object::<O>(..)?; r.alias(other_template, &h)?;` |
| Tempted to add an object so something gets cached? | stop | An object earns canonical storage only with a `canonical` face. No canonical payload means a direct face or raw handler, not a fake object |

Smell test from `providers/DESIGN.md`: repeated "load, derive field, build projection" handlers mean the route family wants an object face; a one-shot query wrapped in object machinery wants a direct face or raw handler.

## Minimal provider skeleton

```rust
use omnifs_sdk::prelude::*;

#[omnifs_sdk::config]
pub struct Config {
    #[serde(default)]
    api_key: String,
}

pub struct State { /* policy, parsed config */ }

#[derive(omnifs_sdk::Endpoint)]
#[endpoint(base = "https://api.example.com")]
#[endpoint(default_header = "Accept: application/json")]
struct Api;

#[omnifs_sdk::path_captures]
struct ItemKey {
    id: u64,
}

#[omnifs_sdk::provider(
    id = "example",
    capabilities(domain("api.example.com", "fetch items")),
    resources(endpoints = [Api]),
)]
impl ExampleProvider {
    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        r.dir("/").handler(|_cx: DirCx<State>| async move {
            Ok(DirProjection::exhaustive([Entry::dir("items")]))
        })?;
        r.file("/items/{id}/title").handler(
            |cx: Cx<State>, key: ItemKey| async move {
                let item: Item = cx.endpoint::<Api>()
                    .get(format!("/items/{}", key.id))
                    .json()
                    .await?;
                Ok(FileProjection::body(item.title).dynamic().build())
            },
        )?;
        Ok(State::from(config))
    }
}
```

`#[provider]` infers config and state from `start`; stateless providers use `fn start(r: &mut Router) -> Result<()>`. The macro seals the router after `start`: overlapping route claims fail initialization with a named-routes error.

## Route template grammar

- Literals: `/items/active`
- Single-segment capture: `/{owner}` (field type's `FromStr` validates)
- Prefix capture: `/@{resolver}`, `/v{version}` (literal prefix stripped before parse)
- Trailing rest capture: `/{*path}` (must be last; captures remaining segments as one string)
- Registration verbs: `r.dir(t).handler(h)`, `r.file(t).handler(h)`, `r.treeref(t).handler(h)`, `r.object::<O>(t, block)`, `r.file_object::<O>(t, block)`, `r.alias(template, &handle)` to mount a detached `object(..)` / `r.object` handle at a second template

Handlers are `async fn(Cx<S>) -> Result<..>`, `async fn(Cx<S>, Key) -> Result<..>` (file/treeref), or `async fn(DirCx<S>) / (DirCx<S>, Key)` (dir). `DirCx` carries the intent: one dir handler serves `Lookup{child}`, `List{cursor}`, and `ReadFile{name}`; check `cx.intent()` when behavior differs, and `cx.page_cursor(1)` for paged listings.

## The object pattern, end to end

`#[omnifs_sdk::object]` emits the whole `impl Object` (the `Key`/`State`/`Canonical` types, `decode`, `kind`) and forwards `load` to a provider-written inherent `async fn load`. `#[path_captures]` emits the `impl Key`. You write `load` (it has callouts) and any `derive` field fns; everything else is derived.

```rust
#[omnifs_sdk::path_captures]
struct IssueKey {
    filter: Facet<StateFilter>,   // route context, NOT identity
    owner: OwnerName,
    repo: RepoName,
    number: u64,
}

#[omnifs_sdk::object(kind = "github.issue", key = IssueKey)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Issue { /* upstream fields */ }

impl Issue {
    // The macro forwards Object::load here (default name Self::load; override
    // with `load = path`). decode defaults to serde_json for a Json canonical.
    async fn load(cx: &Cx<State>, key: &IssueKey, since: Option<Validator>) -> Result<Load<Self>> {
        cx.endpoint::<Api>()
            .get(format!("/repos/{}/{}/issues/{}", key.owner, key.repo, key.number))
            .maybe_if_none_match(since.as_ref())
            .load::<Issue>()        // 200 -> Fresh{value, canonical}, 304 -> Unchanged, 404 -> NotFound
            .await
    }

    fn title(&self, _key: &IssueKey) -> Result<FileProjection> { /* eager field */ }
    fn body(&self, _key: &IssueKey) -> Result<FileProjection> { /* large field */ }
}

impl Representable<Markdown> for Issue { /* whole-object render */ }

r.object::<Issue>("/{owner}/{repo}/issues/{filter}/{number}", |o| {
    o.dynamic();                                  // stability is mandatory
    o.file("item.json").canonical::<Json>()?;     // verbatim stored bytes (== Object::Canonical)
    o.file("item.md").representation::<Markdown>()?; // render from canonical
    o.file("title").derive(Issue::title)?;        // eager leaf from the loaded object
    o.file("body").lazy().derive(Issue::body)?;   // listed, but not eager-preloaded
    o.dir("comments").collection(Issue::comments)?; // child Comment objects
    Ok(())
})?;
```

What the faces are: `canonical::<F>()` is the verbatim stored bytes (exactly one per object; `F` must equal `Object::Canonical`); `representation::<F>()` renders the whole object via `Representable<F>`; `derive(method)` is a field leaf (eager by default, `.lazy()` to skip listing-time preload); `direct`/`blob`/`stream` declare their own read contract; `o.dir(name).collection::<C>(method)` lists child `C` objects, `.choices(names)` a fixed finite name set, `.tree(method)` a subtree handoff. An object with no canonical face (Docker `Container`: only `direct` faces) emits no object-cache entry; a representation/derive face requires a canonical to render from (seal-time error otherwise).

The contract that makes caching work: `Load::Fresh` must carry the **verbatim** upstream bytes plus the validator (ETag). The SDK emits the canonical-store effect; the host stores bytes keyed by the logical id and pushes them back into later `read-file` calls so re-rendering costs no upstream fetch. Because `filter` is a `Facet`, `/issues/open/7` and `/issues/all/7` resolve to one cached object, and finite facet choices expand view leaves across all aliases.

## Collections, preloads, file-shaped objects

A collection lists child objects and decides what the list payload can preload. The list method's return type selects the cursor; the host stores cursor bytes opaquely and echoes them back:

```rust
async fn repos(self_key: OwnerKey, cx: ListCx<GhCursor, State>) -> Result<Collection<Repo, GhCursor>> {
    let page = list_repos(&cx, &self_key.owner, cx.cursor()).await?;
    Ok(Collection::page(page.items.into_iter().map(|row| {
        CollectionEntry::fresh(RepoKey { .. }, Repo::from(row.object), row.canonical)
    })).next(page.next_cursor))
}
```

`CollectionEntry::fresh(key, value, canonical)` stores the child canonical at listing time (use only when the list row satisfies the child's canonical contract byte-for-byte); `::derived(key, files)` projects shallow eager leaves but no canonical; `::key(key)` is discovery only. `Collection::complete` is exhaustive, `::page(..).next(cursor)` has more, `::partial` is intentionally open with no cursor, `::unchanged` matched the listing validator. The list method's key `K` is parsed from the COLLECTION DIR PATH, so a collection under `o.dir("issues/{filter}")` reads `{filter}` from `K`.

`Object::load` can also carry preloads when one fetch materializes siblings (Oura's date-range window): `Load::fresh(value, canonical).preload_object(ObjectEntry::fresh(sibling_key, sibling, sibling_canonical))` (same object type only) and `.preload_file(path, projection)` (inline/deferred sources only). Event handlers return a typed `Invalidation` (`Invalidation::new().listing_path(..).object::<O>(&key)`), which the macro lowers to invalidation effects.

A file-shaped object projects as a single file, not a directory: `r.file_object::<O>(template, |o| { o.dynamic(); o.canonical::<Json>()?; Ok(()) })?` declares exactly one canonical/representation/direct/blob face, and the path itself is the file.

## HTTP, batching, pagination, rate limits

- Conditional loads: `cx.version()` is the host-pushed validator for this anchor; map it with `.maybe_if_none_match(..)`. Object loads get `since` handed to them.
- Two terminal layers on the request builder: object-layer `.load::<T>()`/`conditional` (three-state) vs structural `.json::<T>()`/`.send_checked()` (two-state, errors on non-2xx). Pick deliberately.
- Batch parallel fetches with `join_all([f1, f2, ..])`: one suspension round, host runs them concurrently, results return in order. Every child must come from the same `Cx` and yield exactly one callout per suspension or results silently misalign.
- Pagination: return `DirProjection::open(entries).with_cursor(Cursor::Page(n + 1))`; the host echoes the cursor back on continuation.
- Rate limits: a 429 arms a per-authority breaker (cooldown from `Retry-After` or the endpoint's `rate_limit` policy); further calls fast-fail without a callout until cooldown passes.
- Large bytes stay host-side: `fetch-blob` lands a body in the host blob cache (serve via blob byte-source or `open-archive` into a `TreeRef`); `read-blob` brings ranges across the boundary, sparingly.

## Events and freshness

`events(timer(Duration::from_secs(n), Self::on_tick))` registers `async fn on_tick(cx: Cx<State>) -> Result<Invalidation>`; build the invalidation with `Invalidation::new().listing_path(..).listing_prefix(..).object::<O>(&key)`, which the macro lowers to the host invalidation channel. Non-timer provider events are currently swallowed. A provider that never invalidates and never sets validators serves stale data forever; pick at least one freshness mechanism per dynamic route family.

## Manifest (provider metadata)

The manifest (identity, capabilities, config schema, auth) is authored entirely from `#[omnifs_sdk::provider(...)]` arguments and embedded as the `omnifs.provider-metadata.v1` wasm custom section at build time by `just providers build`. There is no hand-written `omnifs.provider.json`.

- `id = ".."`, `display_name = ".."`, `mount = ".."` set identity.
- `capabilities(domain("v","why"), git_repo("v","why"), unix_socket(dynamic,"why"), preopened_path(dynamic,"why"), memory_mb(<int>,"why"))` declare capability needs; `unix_socket`/`preopened_path` `dynamic` forms resolve at mount-start from a `HostSocket`/`HostFile` config field.
- `auth = <expr>` splices a typed `omnifs_sdk::auth::Auth` builder value (covering `StaticToken`, `OAuth` device-code/PKCE/client-side-token flows, and `Validation`) into the manifest auth block.
- The config schema is derived automatically from the `start` config type (via `#[omnifs_sdk::config]`, which now also derives `schemars::JsonSchema`); no manifest argument is needed.
- Host-resource config fields are typed: `omnifs_sdk::HostFile` (host file → read-only WASI preopen) and `omnifs_sdk::HostSocket` (`unix://` socket); their `JsonSchema` emits an `x-omnifs-resource` marker on the property.

The compact auth wire form (the shape serialized into the embedded section) is unchanged from the former JSON file format; it is now produced from Rust builder expressions instead.

## Build and verify

```bash
cargo build -p <provider-crate> --target wasm32-wasip2          # the component artifact
cargo clippy -p 'omnifs-provider-*' -p 'omnifs-tool-*' -p test-provider --target wasm32-wasip2 -- -D warnings
just providers check                                             # the repo gate
omnifs dev -y                                                    # live container with all builtin providers
docker exec omnifs /bin/zsh -lc 'omnifs status'
```

For any path-surface change, test whole-shell traversal in the live container, not just the leaf: `ll`, `cd`, and `find` from the provider root through every intermediate directory; verify parents do not synthesize duplicate roots, scaffolding names do not bind as captures, and the standard toolbox (`cat`, `grep -r`, `find`, `tar`, `diff`, editors) behaves. That toolbox compatibility list in AGENTS.md is the acceptance bar.

## Top pitfalls

1. Forgetting `Facet` on a filter/alias capture: the same object caches once per alias and invalidation misses siblings.
2. `canonical::<F>()` whose `F` is not `Object::Canonical`, two canonical faces, or a representation/derive face with no canonical: all are seal-time errors. `Load::fresh(value, canonical)` always carries the verbatim body and validator.
3. Inventing objects for query-shaped data (DNS answers, process listings): corrupts cache semantics; stay path-oriented.
4. Inline bytes over 64 KiB or with non-exact size: projection validation rejects it; use deferred reads or blobs.
5. `MemoryRangeReader` for genuinely large content: it buffers everything; implement a real `RangeReader` over blob ranges.
6. Stub handlers for intermediate directories: unnecessary (auto-navigation) and can shadow real routes.
7. Unknown config keys: `#[omnifs_sdk::config]` sets `deny_unknown_fields`; mount JSON typos fail initialization (good); do not "fix" by loosening.
8. Expecting webhook/file-changed events to fire handlers: only `timer` is dispatched today.
9. A finite `choices()` list for a dynamic universe: it silently hides new upstream values; reserve choices for truly static sets.
10. Provider-local caching "to be safe": it fights the host's invalidation and fence machinery; delete it.
11. Face modifiers go before the terminal face call: `o.file("body").lazy().derive(f)` marks `body` lazy; `.derive()`/`.canonical()`/`.direct()`/etc. is the terminal registration call. A collection child object (`o.dir(..).collection::<C>(..)`) must have its own `r.object::<C>` route or seal fails to resolve it.

## Where to look

- `crates/omnifs-sdk/src/lib.rs`: the crate-level authoring guide; module map
- `crates/omnifs-sdk/src/object.rs`, `collection.rs`, `invalidation.rs`, `router/object.rs`: the `Object`/`Key`/`Load`/`Collection`/`Invalidation` surface and the face builder
- `crates/omnifs-sdk/tests/wit_boundary.rs`: canonical end-to-end usage examples (faces driven through the WIT boundary)
- `providers/DESIGN.md`: route-API doctrine and per-provider classification
- `providers/test/src/lib.rs`: SDK conformance fixture (every face exercised)
- `providers/github`: object faces with collections, choices, tree, blob, direct; `providers/arxiv`: blob faces + version facets; `providers/oura`: file-shaped objects with preload; `providers/docker`, `providers/db`, `providers/dns`: direct faces and raw handlers
- `crates/omnifs-wit/wit/provider.wit`: the wire contract underneath everything
- AGENTS.md: repo-wide invariants (toolbox compatibility, caching model, build gates)
