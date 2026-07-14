# Engine and SDK roadmap

Status: future
Scope: unshipped engine, SDK, and provider opportunities that fit the current architecture. Each entry states the need, the cost, and the condition that would justify it. Market and product strategy live elsewhere.
Related: `docs/architecture/00-overview.md` (the invariants these must respect), `async-http.md`, `mutations-via-git.md`.

## Typestate object blocks

The face-validity checks (a representation/derive face with no canonical, two canonical faces, `Live` on a non-stream face) run while finishing the object block and return a named error. The refinement is a builder typestate that makes the invalid combinations compile errors instead, removing the runtime checks. Add that complexity only when compile-time enforcement is worth the builder API cost.

## Cross-type preload_object

`Load::preload_object` is constrained to the requested object's OWN type (Oura's same-type sibling days). The general case is a load teaching the host about any object kind it actually fetched (an issue load preloading its author object), via the object-route registry so the SDK can reach the other type's anchor and view leaves. Add it only when a provider needs cross-kind preloading.

## Full directory enumeration

The `@next` / `@all` host pagination subsystem gives explicit, control-file-driven enumeration of capped directories. Plain `readdir` would need resumable cursors for `find`, `tar`, and `rg` to walk every page without control files. Add that machinery only for a driver that requires a fully enumerable directory; capped listings otherwise stay honestly `open`.

## Field-level validators

Whole-object versioning is the model: one opaque Version per object, the provider folding component validators into it. Add per-field validators and precise change attribution only for a high-churn entity that whole-object versioning serves badly.

## Large object representations as ranged blobs

The whole-file `read-file` path carries only render or canonical bytes, so a `Deferred { Ranged }` object `.raw` currently has no byte source. The resolution is to choose the serving operation: a large object representation materializes as a blob and is served ranged by handle (`open-file` / `read-chunk`), while small canonicals keep crossing the boundary to be rendered. The decision rule (by `ReadMode`, by size threshold, or by an explicit per-object declaration) and what a large object's `.raw` is need to be stated before this is built.

## Database provider depth

The `db` provider is read-only SQLite and path-oriented (schema, indexes, counts, samples, `table.json`). Possible additions are per-row paths (`/tables/{table}/rows/{pk}/`) as treeref-backed subtrees, saved queries, and a `db-query` WIT callout that would let a Postgres backend execute parameterized queries host-side. Row paths and saved queries are provider-local; the Postgres callout is a capability-surface change and needs explicit sign-off.

## arXiv category browsing

Category listings are flat, stateless, and cursor-paged: `/categories/{category}/papers` emits paper-id directories and stores no member canonicals. A live `recent` scan could page `export.arxiv.org` and materialize stable day buckets (`submissions/{YYYYMMDD}`) derived from already-fetched pages. The grain is the Atom `<published>` timestamp paired with `sortBy=submittedDate` (not `<updated>`), bucketed by UTC published date; the feed-level `<updated>` is the snapshot/reset key, and contiguous-prefix completion marks a day fully scanned. This stays path-oriented (no member canonicals) and keeps rate-limit handling in the host HTTP path, not a provider-side sleep loop.

## Cross-object canonical references

Object references are implicit to an entry's own object id. An explicit `canonical-ref(anchor)` form would let one object's canonical reference another's by anchor without duplicating bytes. This is useful only with a concrete cross-type preload consumer.

## Explicit pagination control namespace

Listing honesty plus `@next` / `@all` is the current control-file model. Add a broader `@pages` namespace only if that model proves insufficient.

## Additional native mount frontends

Linux FUSE and macOS NFSv4 loopback both consume `omnifs_engine::namespace`; provider APIs do not depend on either frontend. Any additional native surface, such as FSKit on macOS, must use the same namespace interface and remains product-gated rather than technically blocked.
