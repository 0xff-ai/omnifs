# omnifs architecture

Status: living (tracks the system as built)
Scope: the load-bearing invariants, the decisions behind them, and the directions that are deliberately rejected. This is the "why it is shaped this way" doc. It does not re-derive each subsystem; it ties them together and points to the detailed docs.
Related: `object-cache-primary.md`, `file-attributes.md`, `path-dispatch-and-listing.md`, `object-model-interfaces.md`, `daemon-cli-split.md`, `host-auth.md`; provider authoring lives in `providers/DESIGN.md` and `skills/omnifs-provider-sdk/SKILL.md`; the contract is `crates/omnifs-wit/wit/provider.wit` (`omnifs:provider@0.4.0`).

omnifs projects external services (GitHub, DNS, arXiv, Docker, Linear, a SQL database) as a Linux filesystem. A host runtime loads each provider as a `wasm32-wasip2` component and drives it through the byte-level `omnifs:provider` WIT interface. Every consumer of the projected tree (shells, scripts, editors, agents) is served by the same mount; nothing may assume a privileged consumer.

## 1. The byte boundary (the spine)

The load-bearing decision is where object reasoning lives.

- The **host** knows only paths, bytes, content types, and file attributes, plus opaque caching and the FUSE frontend. It reads a path and gets bytes. It never requests an Object, never parses one, never renders one, and never derives a path-to-object mapping.
- The **provider SDK** (guest side) owns all object reasoning: identity, canonical assembly, rendering into representations, versioning, preload, and revalidation. It hands the host bytes.

Everything else follows from this. The WIT read interface is byte-level; the host caches are opaque byte stores; the host learns the path-to-id relationship only from provider-emitted effects, never by inspecting a path or a payload. Where the SDK needs the host to do something object-adjacent (store a canonical, echo a content type), it emits an effect the host stores and replays byte-for-byte without interpreting it.

## 2. The object model

Two path kinds exist, and the host cannot tell them apart (it sees only file attributes and which byte op it invokes):

- **Object** (a promotion). Content with a single canonical model rendered into one or more content-typed Representations. Reach for it when content renders to multiple representations, the same entity is reachable at multiple paths, or you want object-level revalidation. Object is not the default; it is earned.
- **Structural file** (the default). A single-format file served as direct bytes under the file-attributes contract, with no canonical model behind it. `Stability::Live` content (live logs, `tail -f`, direct IO) can never be an Object: a canonical model is a finite snapshot and a Version cannot be honest for a moving target.

Identity is two-layer, and the layering is the security boundary: `LogicalId = (Object::kind(), normalized non-Facet captures)` is computed in the provider; `ObjectId = (mount, LogicalId)` is formed by the host. Mount-prefixing every object/view/reverse/negative/tombstone key is what stops two GitHub mounts with different credentials from sharing private canonical bytes for the same `owner/repo/number`. The WIT carries `logical-id { kind, captures }`; the host resolves a path to an id by exact map lookup, never a prefix probe (`crates/omnifs-cache/src/object.rs`).

A route Facet (`Facet<T>`) is route context excluded from identity, so several paths can resolve to one cached object. arXiv's version selector (`@latest` / `vN`) and a category prefix are Facets: `/papers/{paper}/@latest`, `/papers/{paper}/vN`, and `/categories/{category}/papers/{paper}/vN` are one object. The canonical bytes are exactly the verbatim upstream response body (never `serde(Self)`); `.raw` (and any representation whose content type is the canonical's own) is served from those bytes without calling `render`. A canonical may be assembled from several upstream calls into one blob; the provider folds the component validators into one opaque Version.

## 3. Caches

The host owns storage as plain bytes. It owns no semantics. There are exactly three caches, named by role (full design: `object-cache-primary.md`).

- **Object cache** (`object.redb`): the durable primary. Canonical upstream bytes keyed by logical id, mount-prefixed, global. This inverts the prior model where a durable browse/view tier was primary and the canonical store was in-memory (`crates/omnifs-cache/src/object.rs`).
- **View cache** (`view.redb`): derived and disposable. It is deleted and recreated unconditionally on every startup, so stale rendered bytes can never survive a restart to disagree with the durable object cache (`crates/omnifs-cache/src/view.rs`; opened in `lib.rs`). It holds the rendered representations and dirents shell tools read, recomputed from the object cache with no upstream refetch.
- **Blob cache**: large binary served by handle; its bytes never cross to the provider (section 7).

Read path: on a view miss the host resolves the path to a logical id by exact map lookup and **pushes** the cached canonical (`canonical-input { id, validator, bytes }`) into `read-file`; the SDK self-checks the pushed id against its route-derived id and renders without an upstream call (`crates/omnifs-sdk/src/router/object.rs`). There is no `canonical-read` callout; the host pushes, the provider never pulls. Identity representations (`byte-source::canonical`) live only in the object cache and are never copied into the view cache.

Eviction and coherence: there is **no TTL on canonical bytes**; an object entry leaves only by capacity eviction or explicit invalidation, and capacity eviction drops the canonical and its validator atomically (no stranded validator). Object overwrite drops the object's prior rendered view leaves but unions (never removes) its alias set; `invalidate object(id)` is the only operation that removes path-to-id entries. View leaves, by contrast, **do** carry a `Stability`-derived freshness deadline: `Dynamic` is `now + 3000ms`, `Live` is `now` (immediate), `Stable` has none (`crates/omnifs-host/src/materialize.rs`, `clock.rs`; `crates/omnifs-cache/src/lib.rs`). So "no TTL" is precise for the object cache and wrong for the view layer; providers still add no TTLs or LRUs of their own.

A per-mount **generation + tombstone fence** rejects a write derived from data read before a concurrent invalidation. It is in-memory and resets on restart; resetting the generation to 0 is safe because the only thing it fences (rendered view bytes, in-flight loads) does not survive restart, and a lost tombstone self-corrects via `Dynamic` revalidation (`crates/omnifs-cache/src/lib.rs`).

## 4. Effects: the only host-mutation channel

A provider step either **suspends** on a non-empty batch of callouts or **returns** a result with effects. A returned step never carries trailing callouts; there are no fire-and-forget callouts (`provider-step` is two arms, `suspended` / `returned`, `provider.wit`). The five callouts are `fetch`, `git-open-repo`, `fetch-blob`, `open-archive`, `read-blob`; every one expects a matching result via `resume`.

Terminal host mutations travel only as `effects`, which has exactly three blocks (`provider.wit`):

- `canonical` STOREs raw upstream bytes into the object cache (`canonical-store { id, validator, bytes, view-leaves }`); `view-leaves` are the absolute paths the host indexes for that id.
- `fs` WRITEs materialized files/dirs into the view cache.
- `invalidations` are `object(logical-id)` or `listing(path | prefix)`.

An `error` terminal must carry empty effects; the host rejects an error that also requests mutation (`op_lifecycle` `validate_return`). New terminal mutations add explicit effects fields; they never tunnel through callouts. `Load::NotFound` is a cacheable state, not an error; `Err` is reserved for transient or transport failures, so call sites never disambiguate "gone" from "try again" (`crates/omnifs-sdk/src/object.rs`).

## 5. The read path

There is one byte-level read operation from the host's point of view: no provider-to-host canonical-read callout, no host-side render op. `read-file(id, path, content-type, cached-canonical)` materializes whole-file `Deferred(Full)` content; ranged `Deferred(Ranged)` content uses `open-file` / `read-chunk` / `close-file`; inline projected bytes are served from cache without calling the provider. When the requested content type is the canonical's own (or `.raw`), the SDK serves the stored bytes verbatim and never calls `render`; otherwise it reconstructs `Self` via `parse_canonical` and renders. This is why there is no `Render`/`Raw` coherence hole.

Revalidation is **signal-driven, not TTL-driven**: with no per-read TTL, a conditional upstream fetch fires only when a staleness signal (an `on-event`, a webhook, a poller) has marked the path since the last read. The opaque Version maps to a transport validator (an ETag to `If-None-Match`); a 304 (`Load::Unchanged`) renders from the pushed bytes and emits no canonical write, a change yields a new canonical and Version. Today only `timer-tick` events dispatch to a handler, declared via `events(timer(..))` (`crates/omnifs-host/src/registry.rs`); `webhook-received` / `file-changed` / `auth-refreshed` are defined in the WIT but not yet host-driven.

## 6. Dispatch and listing honesty

The router (`crates/omnifs-sdk/src/router/`) makes any registered route's literal-segment prefix an auto-navigable directory, so authors write no stub handlers for intermediate nodes. Per-segment capture validators participate in match candidacy: a parse rejection falls through to the next-most-specific candidate route, not to ENOENT (this is what keeps `/dns/_reverse/not-an-ip` from matching and erroring at read time). A literal file route outranks a same-depth capture dir at lookup; `read-file` resolves `<anchor>/<stem>.<ext>` to the object at the parent plus the content type from the extension. The macro seals the router after `start`; overlapping leaf claims fail initialization.

Listing honesty is a correctness invariant, not a nicety:

- A listing is `exhaustive` only when the provider actually enumerated every entry. A hard cap yields `open` (non-exhaustive) with no cursor; a cursor is emitted only when the handler can resume from it.
- `lookup` is the authoritative name oracle; `readdir` may be non-exhaustive. An exhaustive listing over a truncated set is a lie that can turn a valid path into ENOENT.
- A directory listing merges the literal sibling routes registered at that depth with the handler's enumeration; an auto-navigable listing is non-exhaustive whenever a capture sibling exists at the next depth.

The `@` prefix is reserved for host-owned control and metadata namespaces; provider listings must never yield an `@`-prefixed child (`crates/omnifs-host/src/pagination.rs`; skipped with a warning in `crates/omnifs-fuse/src/listing.rs`). Full enumeration with real resumable cursors shipped as the host `@next` / `@all` control-file pagination subsystem (`crates/omnifs-host/src/pagination.rs`); it synthesizes a control only when a real resume cursor exists.

## 7. Blobs and archives

Large binary content the host stores and serves by handle; the defining property is that the bytes never cross to the provider. The SDK orchestrates by handle: `fetch-blob` (fetch a URL into the blob store, get a `blob-id`, the only `blob-id` producer), `open-archive` (mount a stored blob as a bind-mounted tree, returns a `tree-ref`), `read-blob` (bring a range across, used sparingly). Archive trees are materialized by an `ExtractKey { cache_key, format, strip_prefix }` and a `TreeMaterializer` that extracts into a temp dir and publishes by atomic rename (`crates/omnifs-host/src/tools/archive.rs`; staging in `crates/omnifs-host/src/sandbox/preopen.rs`). Tree-refs and blob-ids are runtime-local handles, not durable cache keys.

Subtree handoff is result data, not a host mutation: a `subtree(tree-ref)` lookup/list terminal is resolved by the host to a bind-mounted clone directory. The only subtree mechanism is `r.treeref(template).handler(..)` plus plain full-path routes (registering the same handler under, for example, `issues/.../comments` and `pulls/.../comments`); the dedicated subtree/nest abstraction was deliberately dropped in favour of the simpler end-to-end flow. The GitHub `repo/` tree is a read-only treeref over a host-bind-mounted clone, not a writable island.

## 8. File attributes

Every projected file carries `FileAttrs { size, stability, version }` (bytes are deliberately not an attribute; byte availability is a separate `ProjBytes` / WIT `byte-source`). The structural rules bind (enforced in `crates/omnifs-sdk/src/file_attrs.rs` and re-checked host-side in `crates/omnifs-core/src/view.rs`):

- `Stability::Live` requires `Bytes::Deferred { read: Ranged }`; inline and whole-file reads cannot model bytes that change mid-observation. The SDK typestate makes `.live()` reachable only on a `Ranged` builder.
- `Bytes::Inline` requires `Size::Exact(len)`, capped at 64 KiB; the aggregate eager-byte cap per terminal is 512 KiB.
- Unknown and NonZero sizes report a truthful `st_size = 1` sentinel (never 0, which makes tools skip the file as empty; never a large fake value, which breaks `tail -n` / `tar c`). The exact size is learned and promoted on a complete read; unknown-size files open with `FOPEN_DIRECT_IO` so the page cache cannot serve a truncated read against the sentinel.
- Stat-size and read-termination are decoupled: read termination never depends on a stat-size guess. Sizes are learned only by flowing real read bytes through the cache; there is no HEAD-probing on stat.

Full model and the byte-source-to-handler pairing: `file-attributes.md`.

## 9. Observability (inspector)

The inspector is a typed event stream, not a tracing backend. The hot-path `emit` never awaits, never locks for more than a few ns, and never allocates beyond the single `Arc<InspectorRecord>` it already holds; the only blocking path (an opt-in file tee) is off by default (`crates/omnifs-host/src/inspector.rs`). History is a wait-free `ArrayQueue` (cap 1024); on full, `emit` pops the oldest and counts it. Live fan-out is a tokio broadcast (cap 256); a slow subscriber gets `Lagged(n)` and resumes from latest rather than blocking the producer. A daemon-local monotonic `seq` de-dups the overlap window between the history snapshot and the broadcast subscription. The transport is `GET /v1/events` (newline-framed JSON), surfaced by the daemon control API.

Explicit non-goal: do not migrate to `tracing::instrument` / `tracing-subscriber` / OpenTelemetry. Both lose the typed-enum guarantees and add dependency weight for no transport feature gained. Open: subscriber authentication for the non-dev path (the control API is unauthenticated local-trust today) and a hard total-order guarantee under concurrent multi-thread emit (the contract is "approximately emission order; sort by `seq`").

## 10. Credentials and auth

Providers are untrusted WASM and never hold tokens. The host attaches credentials after the callout crosses the WASM boundary, and owns auth, caching, and rate limits; provider capabilities are declared in `omnifs.provider.json` (embedded as `omnifs.provider-metadata.v1`). Capabilities come from the manifest, not from runtime `RequestedCapabilities`: the host reads `domains` / `max_memory_mb` from the manifest; only `needs_git`, additive `unix_sockets`, and `refresh_interval_secs` come from runtime caps.

The production credential backend is the file store at `~/.omnifs/credentials.json`; the daemon reads and writes the same file through the writable `OMNIFS_HOME` bind. The only public wire form is `CredentialId::storage_key()` = `provider:scheme:account` (`crates/omnifs-core/src/auth.rs`); `CredentialEntry`'s secret is private, read via `access_token()`. Refresh coalescing is in-process (an `async_singleflight::Group` keyed by the storage key); `FileStore` uses a sidecar `.lock` (`fs2` flock) for the read-modify-write of `credentials.json`. There is no cross-process refresh protocol. Full design: `host-auth.md`.

## 11. Rejected directions (do not reintroduce)

These were tried, designed against, or explicitly ruled out, and still bound the system:

- No host-side Object semantics, no host `ObjectCache` keyed by provider object identity, no host-side representation rendering. The host stores canonical bytes as opaque.
- No `fetch-placement` / `render-leaves` / `canonical-read` public protocol ops, and no dual dispatch fork (`OMNIFS_DISPATCH`). There is one byte-level read path.
- No fake resumable cursors and no `exhaustive` claim over a truncated set (section 6).
- No provider-owned content caches or TTLs; the host owns all caching, evicting only by capacity or explicit invalidation.
- No writable projected read-model files as an implicit mutation mechanism. Writes, when they land, are explicit, atomic, and auditable (see the future roadmap).
- No macFUSE / `diskutil` / macOS-specific mount behaviour. The FUSE frontend is Linux-only; a read-only NFSv4 loopback frontend (`crates/omnifs-nfs`) now serves stock macOS and Linux host-native, with FSKit a later horizon.
- No `canonical = serde(Self)`; the canonical is the verbatim upstream body.
- WIT changes break every built provider until a negotiation story exists; rebuild all providers and update affected docs in the same change. Breakage is expected in alpha; the rule is keeping providers and docs in step, not preserving the contract.

xattrs (`Object::xattrs` / `upstream_url`) were deliberately dropped in v1 rather than orphaned, with a forward-compatible restore path designed (a single opaque `xattrs` field on `canonical-store` the host would echo on `getxattr` without parsing keys). This records the deliberate scope cut and the re-entry design.

## 12. Decision log

The past decisions that still govern the system today, condensed (each lives in the section above with its code cite):

- Object reasoning lives entirely SDK-side; the host is a byte-level, path-keyed cache (sections 1, 3).
- The object cache is the durable primary; the view cache is disposable and recreated every startup (section 3).
- The host pushes the canonical into `read-file`; there is no canonical-read callout (sections 3, 5).
- Identity is two-layer and mount-prefixed; Facet captures are excluded from identity so aliases collapse to one canonical (section 2).
- Canonical bytes are the verbatim upstream body; identity representations are served without rendering (sections 2, 5).
- Effects are the only terminal host-mutation channel, three blocks, error carries none (section 4).
- `Stability` drives the view-leaf deadline; the object cache itself is deadline-less (section 3).
- The fence is runtime-only and resets on restart (section 3).
- Listing honesty: exhaustive only when enumerated, cap becomes open, never a fake cursor (section 6).
- Object is an optional promotion; structural files are first-class and the default; Live is always structural and ranged (sections 2, 8).
- Capabilities come from the provider manifest, not runtime caps (section 10); providers never hold tokens (section 10).
