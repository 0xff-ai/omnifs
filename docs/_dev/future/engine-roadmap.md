# Engine and SDK roadmap

Status: roadmap (none of this is shipped)
Scope: concrete engine, SDK, and provider opportunities that are deferred but coherent with the current architecture. Each entry states what it is, why it waits, and the open piece. Market and product strategy live elsewhere; this is the technical backlog.
Related: `architecture.md` (the invariants these must respect), `async-http.md`, `mutations-via-git.md`.

These items were consolidated from the SDK-redesign design corpus once that redesign shipped. They survive here because they are real, scoped, and still wanted; the planning documents that carried them did not, because their vocabulary had gone stale against the implemented system.

## Aliases and cross-path object collapse

Multiple routes can already resolve to one cached object through Facet captures excluded from identity (`architecture.md` §2). What is deferred is the general case: an explicit alias channel so the SDK can declare "these paths share one canonical" and the host can index it without inspecting bytes (which the byte boundary forbids). The only current alias source is GitHub's state filter, deferred so issues project on one anchor. The open piece is the alias-anchor channel itself: an SDK-emitted effect the host indexes, plus the rule that separates identity captures from view captures so the path tuple's non-rename-stability does not corrupt identity.

## Preload anchor derivation

Returning a related object's canonical alongside a fetch (an issue's author and labels) so later reads hit cache is the preload mechanism, and the storage side (a `canonical-store` at the related anchor) is settled. The open piece is authoring ergonomics: `cx.preload(&value)` cannot reach the related object's anchor from a bare value, and silently no-ops when the related type is unbound. The fix is a typed `(Key, value)` or route-handle based preload with observable bound-ness, plus a reverse-routing primitive (`cx.build_path`) so a typed handle plus captures yields the anchor.

## Full directory enumeration

The `@next` / `@all` host pagination subsystem gives explicit, control-file-driven enumeration of capped directories. What remains deferred is real resumable cursors threaded through plain `readdir` so `find`, `tar`, and `rg` walk every page of a large directory without the control files. This waits for a driver that genuinely needs a single directory fully enumerable; until then capped listings stay honestly `open`.

## Field-level validators

Whole-object versioning is the model: one opaque Version per object, the provider folding component validators into it. Per-field (per-component) validators for high-churn entities, with precise change attribution, are a possible refinement, out of scope until a driver appears that the whole-object grain serves badly.

## Large object representations as ranged blobs

The whole-file `read-file` path carries only render or canonical bytes, so a `Deferred { Ranged }` object `.raw` currently has no byte source. The resolution is to choose the serving operation: a large object representation materializes as a blob and is served ranged by handle (`open-file` / `read-chunk`), while small canonicals keep crossing the boundary to be rendered. The decision rule (by `ReadMode`, by size threshold, or by an explicit per-object declaration) and what a large object's `.raw` is need to be stated before this is built.

## Database provider depth

The `db` provider is read-only SQLite and path-oriented today (schema, indexes, counts, samples, `table.json`). The deferred surface: per-row paths (`/tables/{table}/rows/{pk}/`) as treeref-backed subtrees, saved queries, and a `db-query` WIT callout that would let a Postgres backend execute parameterized queries host-side. The row-path and saved-query work is provider-local; the Postgres callout is a capability-surface change and needs explicit sign-off.

## arXiv category browsing

Category listings today are flat, stateless, and cursor-paged: `/categories/{category}/papers` emits paper-id directories and stores no member canonicals. The deferred opportunity is a live `recent` scan that pages `export.arxiv.org` and materializes immutable day buckets (`submissions/{YYYYMMDD}`) derived from already-fetched pages. The grain is the Atom `<published>` timestamp paired with `sortBy=submittedDate` (not `<updated>`), bucketed by UTC published date; the feed-level `<updated>` is the snapshot/reset key, and contiguous-prefix completion marks a day fully scanned. This stays path-oriented (no member canonicals) and keeps rate-limit handling in the host HTTP path, not a provider-side sleep loop.

## Cross-object canonical references

Object references are implicit to an entry's own object id today. An explicit `canonical-ref(anchor)` form would let one object's canonical reference another's by anchor without duplicating bytes, which is the substrate the alias and preload work both lean on. Deferred until aliases land.

## Rate-limit unification

GitHub fetches objects via the endpoint path (no rate-limit detection) while structural paths are rate-limit-aware, so the two HTTP paths handle 429s differently. The fix is a rate-limit-aware endpoint terminal that unifies them on the existing breaker (`crates/omnifs-sdk/src/rate_limit.rs`), so object reads get the same backoff structural reads already have.

## Explicit pagination control namespace

`@pages` was a sketched explicit-pagination control namespace, parked in favour of listing honesty plus `@next` / `@all`. It is recorded here as a deliberately-deferred idea, not an active plan; revisit only if the control-file model proves insufficient.

## Archive world convergence

`omnifs:tool-archive` is a separate WIT world from `omnifs:provider`. Whether the archive extraction tool should converge with the provider world (one contract, one sandbox model) rather than stay a sibling is an open structural question, low priority while the two surfaces are stable.

## Native mount frontends

The detailed design for additional frontends (NFSv4 loopback, FSKit on macOS) and the distributed frontend-runtime split lives in the internal design notes; the architecture already keeps FUSE as one frontend of the projected tree behind the `omnifs_host::Namespace` seam, so no provider-facing API depends on the frontend. This is product-gated, not technically blocked.
