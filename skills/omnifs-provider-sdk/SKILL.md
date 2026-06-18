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
6. **Captures validate syntax only.** Existence is decided by handlers, lookup intent, or `Key::load`. Exception: a finite `choices()` set when the universe is truly static or a dynamic route would otherwise synthesize false anchors (the DB provider's table-name case).
7. **Attribute honesty.** Inline bytes require `Size::Exact(len)` and are capped at 64 KiB (`MAX_PROJECTED_BYTES`); bigger or unknown content is deferred. `Stability::Live` requires `Deferred { read: Ranged }` (the validator rejects anything else). Dynamic upstream = dynamic projection; versioned upstream = stable projection.
8. **Identity vs facets.** A key's non-`Facet` fields, in declaration order, ARE the object's identity (its logical id and cache key). Route-context captures that must not split the cache (list filter, version selector, team alias) are `Facet<T>`. Changing an object's `kind` string or identity captures orphans every cached object.
9. **Auth never appears in provider code.** Credentials are declared in `omnifs.provider.json` and materialized into requests by the host. `#[endpoint(auth = ..)]` is rejected at compile time by design.
10. **Reuse the SDK, stdlib, and imported crates before writing plumbing.** Check `omnifs-sdk` projections, endpoint helpers, object/load APIs, typed captures, `time`, `url`, `serde_json`, `strum`, and other already-present workspace dependencies before adding local parsers, date math, enum tables, HTTP glue, cache shims, or projection helpers.
11. **Use `strum` for finite enum path segments.** Derive `EnumString`, `AsRefStr`, `Display`, and usually `VariantArray` with `serialize_all = "snake_case"` instead of hand-writing parse/display ladders or parallel `&[&str]` tables. Put the single universe on the enum (`VariantArray::VARIANTS` or an enum-owned `ALL`), and derive filenames, entries, and `PathSegment::choices` from that one list.
12. **Use `hashbrown::HashMap`** (re-exported by the SDK) for provider-internal maps.
13. **Error semantics:** `NotFound` for absent upstream resources or rows; `InvalidInput` for impossible path syntax; `rate_limited(..).with_retry_after(..)` on 429 (drives the SDK breaker and host window); `from_http_status` for the rest. Put operation context in the message.
14. **No provider unit tests in-crate.** Verify behavior through host-driven integration tests and the live `omnifs dev` container (repo policy).

## Which SDK flavour: the decision table

Decide per route family, not per provider; hybrids are normal (arxiv, github).

| Question about the path family | Answer | Use |
|---|---|---|
| Is there ONE canonical upstream payload (an issue, a paper, a PR) serving several derived leaves (`title`, `body`, `item.json`)? | yes | **Object-oriented**: `r.object::<O>(template, \|o\| ..)` with `#[omnifs_sdk::object]` + a `#[path_captures]` key implementing `Key::load` |
| Is the data a query result or operational state with no durable object behind it (DNS answer, `docker ps`, a DB row read, live metrics)? | yes | **Path-oriented**: `r.dir`/`r.file` handlers returning fresh bytes with honest stability; emit NO canonical-store effects |
| Is the subtree a real tree the host can materialize wholesale (git repo, release archive)? | yes | **Treeref handoff**: `r.treeref(template).handler(..)` returning `TreeRef`; provider dispatch stops there |
| Is a leaf a dynamic alias of a versioned resource (`@latest` vs `vN`)? | yes | Version selector as a `Facet` route capture; numbered versions stable, the alias dynamic (arxiv pattern) |
| Is it a list/discovery route whose upstream payload already carries object fields? | yes | Path-oriented discovery handler that eager-projects the object leaves it can vouch for, deferred files for the rest (github/linear pattern) |
| Tempted to add an object so something gets cached? | stop | If there is no canonical payload, a fake object corrupts the cache model. Path-oriented + honest stability is correct |

Smell test from `providers/DESIGN.md`: repeated "load, derive field, build projection" handlers mean the route family wants an object; a one-shot query wrapped in object machinery means it wants a plain handler.

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
    metadata = "omnifs.provider.json",
    resources(endpoints = [Api]),
)]
impl ExampleProvider {
    type Config = Config;
    type State = State;

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

`type Config` defaults to `NoConfig`, `type State` to `()`; `start` may omit the config parameter. The macro seals the router after `start`: overlapping route claims fail initialization with a named-routes error.

## Route template grammar

- Literals: `/items/active`
- Single-segment capture: `/{owner}` (field type's `FromStr` validates)
- Prefix capture: `/@{resolver}`, `/v{version}` (literal prefix stripped before parse)
- Trailing rest capture: `/{*path}` (must be last; captures remaining segments as one string)
- Registration verbs: `r.dir(t).handler(h)`, `r.file(t).handler(h)`, `r.treeref(t).handler(h)`, `r.object::<O>(t, block)`, `r.file_object::<O>(t, block)`, `r.attach(prefix, &handle)` for a detached `object(..)` handle
- `.desc("..")` attaches a human-readable description to any route or leaf, surfaced in the introspected route table (`omnifs.routes.json`, doc/CLI rendering); it carries no dispatch meaning. On path routes call it before `.handler(..)` (`r.dir(t).desc("..").handler(h)`); inside an object block `o.desc("..")` describes the object route itself and `o.file("title").desc("..").project(..)` / `o.dir("comments").desc("..").handler(..)` describe leaves. Keep descriptions short and agent-legible.

Handlers are `async fn(Cx<S>) -> Result<..>`, `async fn(Cx<S>, Key) -> Result<..>` (file/treeref), or `async fn(DirCx<S>) / (DirCx<S>, Key)` (dir). `DirCx` carries the intent: one dir handler serves `Lookup{child}`, `List{cursor}`, and `ReadFile{name}`; check `cx.intent()` when behavior differs, and `cx.page_cursor(1)` for paged listings.

## The object pattern, end to end

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

impl Key for IssueKey {
    type Object = Issue;
    type State = State;
    async fn load(&self, cx: &Cx<State>, since: Option<Validator>) -> Result<Load<Issue>> {
        cx.endpoint::<Api>()
            .get(format!("/repos/{}/{}/issues/{}", self.owner, self.repo, self.number))
            .maybe_if_none_match(since.as_ref())
            .load::<Issue>()        // 200 -> Fresh{value, canonical}, 304 -> Unchanged, 404 -> NotFound
            .await
    }
}

r.object::<Issue>("/{owner}/{repo}/issues/{filter}/{number}", |o| {
    o.desc("A GitHub issue");                   // describes the object route itself
    o.representations("item", (Markdown,))?;   // item.md (+ item.json from canonical)
    o.file("title").desc("The issue title").project(Issue::title)?; // eager leaf
    o.file("body").lazy().project(Issue::body)?; // listed, but not eager-preloaded
    o.dir("comments").handler(IssueKey::comments)?; // dynamic child under the object
    Ok(())
})?;
```

The contract that makes caching work: `Load::Fresh` must carry the **verbatim** upstream bytes plus the validator (ETag). The SDK emits the canonical-store effect; the host stores bytes keyed by the logical id and pushes them back into later `read-file` calls so re-rendering costs no upstream fetch. `Load::fresh(value)` (empty canonical) skips durable caching; use it only when there is deliberately nothing to cache. Because `filter` is a `Facet`, `/issues/open/7` and `/issues/all/7` resolve to one cached object, and finite facet choices expand view leaves across all aliases.

## HTTP, batching, pagination, rate limits

- Conditional loads: `cx.version()` is the host-pushed validator for this anchor; map it with `.maybe_if_none_match(..)`. Object loads get `since` handed to them.
- Two terminal layers on the request builder: object-layer `.load::<T>()`/`conditional` (three-state) vs structural `.json::<T>()`/`.send_checked()` (two-state, errors on non-2xx). Pick deliberately.
- Batch parallel fetches with `join_all([f1, f2, ..])`: one suspension round, host runs them concurrently, results return in order. Every child must come from the same `Cx` and yield exactly one callout per suspension or results silently misalign.
- Pagination: return `DirProjection::open(entries).with_cursor(Cursor::Page(n + 1))`; the host echoes the cursor back on continuation.
- Rate limits: a 429 arms a per-authority breaker (cooldown from `Retry-After` or the endpoint's `rate_limit` policy); further calls fast-fail without a callout until cooldown passes.
- Large bytes stay host-side: `fetch-blob` lands a body in the host blob cache (serve via blob byte-source or `open-archive` into a `TreeRef`); `read-blob` brings ranges across the boundary, sparingly.

## Events and freshness

`events(timer(Duration::from_secs(n), Self::on_tick))` registers `async fn on_tick(cx: Cx<State>) -> Result<Effects>`; emit invalidations there (`Effects` carries `invalidations` for objects and listing paths/prefixes). Non-timer provider events are currently swallowed with empty effects. A provider that never invalidates and never sets validators serves stale data forever; pick at least one freshness mechanism per dynamic route family.

## Manifest (`omnifs.provider.json`)

Lives at the crate root, named in `metadata = "omnifs.provider.json"`, validated at compile time, embedded as the `omnifs.provider-metadata.v1` custom section. It declares: `id`, `displayName`, `defaultMount`, `capabilities` (each domain with a `why` string; private IPs are blocked by default; plain HTTP is always denied), and `auth` (scheme, flows). Editing the JSON alone is safe: the macro tracks it as a build input.

## Build and verify

```bash
cargo build -p <provider-crate> --target wasm32-wasip2          # the component artifact
cargo clippy -p 'omnifs-provider-*' -p 'omnifs-tool-*' -p test-provider --target wasm32-wasip2 -- -D warnings
just providers-check                                             # the repo gate
omnifs dev -y                                                    # live container with all builtin providers
docker exec omnifs /bin/zsh -lc 'omnifs status'
```

For any path-surface change, test whole-shell traversal in the live container, not just the leaf: `ll`, `cd`, and `find` from the provider root through every intermediate directory; verify parents do not synthesize duplicate roots, scaffolding names do not bind as captures, and the standard toolbox (`cat`, `grep -r`, `find`, `tar`, `diff`, editors) behaves. That toolbox compatibility list in AGENTS.md is the acceptance bar.

## Top pitfalls

1. Forgetting `Facet` on a filter/alias capture: the same object caches once per alias and invalidation misses siblings.
2. `Load::fresh(value)` with empty canonical bytes: silently opts out of durable caching; pass the verbatim body.
3. Inventing objects for query-shaped data (DNS answers, process listings): corrupts cache semantics; stay path-oriented.
4. Inline bytes over 64 KiB or with non-exact size: projection validation rejects it; use deferred reads or blobs.
5. `MemoryRangeReader` for genuinely large content: it buffers everything; implement a real `RangeReader` over blob ranges.
6. Stub handlers for intermediate directories: unnecessary (auto-navigation) and can shadow real routes.
7. Unknown config keys: `#[omnifs_sdk::config]` sets `deny_unknown_fields`; mount JSON typos fail initialization (good); do not "fix" by loosening.
8. Expecting webhook/file-changed events to fire handlers: only `timer` is dispatched today.
9. A finite `choices()` list for a dynamic universe: it silently hides new upstream values; reserve choices for truly static sets.
10. Provider-local caching "to be safe": it fights the host's invalidation and fence machinery; delete it.
11. Put projected-leaf modifiers (`.lazy()`, `.desc("..")`) before `.project()`/`.handler(..)`: `o.file("body").lazy().desc("..").project(f)`; `.project()`/`.handler(..)` is the terminal registration call. On path routes, `.desc("..")` likewise precedes the terminal `.handler(..)`.

## Where to look

- `crates/omnifs-sdk/src/lib.rs`: the crate-level authoring guide; module map
- `providers/DESIGN.md`: flavour doctrine and per-provider classification
- `providers/test/src/lib.rs`: SDK conformance fixture (every feature exercised)
- `providers/github`, `providers/linear`: object-oriented exemplars; `providers/dns`, `providers/docker`, `providers/db`: path-oriented; `providers/arxiv`: hybrid with version facets
- `crates/omnifs-wit/wit/provider.wit`: the wire contract underneath everything
- AGENTS.md: repo-wide invariants (toolbox compatibility, caching model, build gates)
