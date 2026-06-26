# Engine and SDK roadmap

Status: roadmap (none of this is shipped)
Scope: concrete engine, SDK, and provider opportunities that are deferred but coherent with the current architecture. Each entry states what it is, why it waits, and the open piece. Market and product strategy live elsewhere; this is the technical backlog.
Related: `docs/architecture/00-overview.md` (the invariants these must respect), `async-http.md`, `mutations-via-git.md`.

These items were consolidated from the SDK-redesign design corpus once that redesign shipped. They survive here because they are real, scoped, and still wanted; the planning documents that carried them did not, because their vocabulary had gone stale against the implemented system.

## Shipped in the object SDK redesign

The provider object SDK redesign (`providers/DESIGN.md`) landed the one-route-API surface and removed several entries that this roadmap previously carried as deferred. These are done, recorded so they are not re-planned:

- **Faces under file/dir.** `canonical`, `representation`, `derive` (eager/lazy), `object`, `direct`, `blob`, `stream`, `tree`, `collection`, `choices`, `children` (`crates/omnifs-sdk/src/router/object.rs`).
- **Explicit aliases.** `r.alias(template, &handle)` mounts one object spec at a second template (the alias-anchor channel; identity captures stay separate from facets).
- **Typed collections.** `Collection<C, Cur>` with `CollectionEntry::fresh/derived/key` and host-opaque typed cursors; a `fresh` entry stores the child canonical at listing time (`crates/omnifs-sdk/src/collection.rs`).
- **Typed preloads / preload anchor derivation.** `Load::preload_object(ObjectEntry::fresh(..))` and `.preload_file(path, proj)` reach the sibling's anchor from the entry key against the registered route, replacing hand-built effect plumbing.
- **File-shaped object anchors.** `r.file_object::<O>` projects an object as a single file.
- **Tree face.** `o.dir(name).tree(method)` registers a subtree handoff as an object dir face.

## Typestate object blocks

The face-validity checks (a representation/derive face with no canonical, two canonical faces, `Live` on a non-stream face) run at seal time and panic with a named error. The refinement is a builder typestate that makes the invalid combinations compile errors instead, removing the runtime checks. This is a builder refactor with no behavior change; deferred so the migration could land first.

## Cross-type preload_object

`Load::preload_object` is constrained to the requested object's OWN type (Oura's same-type sibling days). The general case is a load teaching the host about any object kind it actually fetched (an issue load preloading its author object), via the object-route registry so the SDK can reach the other type's anchor and view leaves. Deferred until a provider needs it.

## Preload sibling listing-visibility

A range-fetched preload (Oura's neighboring days) is resolvable by lookup but does not yet appear in the parent `ls` before it is looked up, because `lower_preloads` does not emit the `project_dir`/`project_file` listing entries the old hand-built path did. The fix is to optionally emit those listing effects so range-fetched siblings are visible in a directory listing immediately. Deferred as a non-exhaustive-listing nicety, not a correctness gap.

## Full directory enumeration

The `@next` / `@all` host pagination subsystem gives explicit, control-file-driven enumeration of capped directories. What remains deferred is real resumable cursors threaded through plain `readdir` so `find`, `tar`, and `rg` walk every page of a large directory without the control files. This waits for a driver that genuinely needs a single directory fully enumerable; until then capped listings stay honestly `open`.

## Field-level validators

Whole-object versioning is the model: one opaque Version per object, the provider folding component validators into it. Per-field (per-component) validators for high-churn entities, with precise change attribution, are a possible refinement, out of scope until a driver appears that the whole-object grain serves badly.

## Large object representations as ranged blobs

The whole-file `read-file` path carries only render or canonical bytes, so a `Deferred { Ranged }` object `.raw` currently has no byte source. The resolution is to choose the serving operation: a large object representation materializes as a blob and is served ranged by handle (`open-file` / `read-chunk`), while small canonicals keep crossing the boundary to be rendered. The decision rule (by `ReadMode`, by size threshold, or by an explicit per-object declaration) and what a large object's `.raw` is need to be stated before this is built.

## Database provider depth

The `db` provider is read-only SQLite and path-oriented today (schema, indexes, counts, samples, `table.json`). The deferred surface: per-row paths (`/tables/{table}/rows/{pk}/`) as treeref-backed subtrees, saved queries, and a `db-query` WIT callout that would let a Postgres backend execute parameterized queries host-side. The row-path and saved-query work is provider-local; the Postgres callout is a capability-surface change and needs explicit sign-off.

## arXiv category browsing

Category listings today are flat, stateless, and cursor-paged: `/categories/{category}/papers` emits paper-id directories and stores no member canonicals. The deferred opportunity is a live `recent` scan that pages `export.arxiv.org` and materializes stable day buckets (`submissions/{YYYYMMDD}`) derived from already-fetched pages. The grain is the Atom `<published>` timestamp paired with `sortBy=submittedDate` (not `<updated>`), bucketed by UTC published date; the feed-level `<updated>` is the snapshot/reset key, and contiguous-prefix completion marks a day fully scanned. This stays path-oriented (no member canonicals) and keeps rate-limit handling in the host HTTP path, not a provider-side sleep loop.

## Cross-object canonical references

Object references are implicit to an entry's own object id today. An explicit `canonical-ref(anchor)` form would let one object's canonical reference another's by anchor without duplicating bytes. Aliases and typed preloads shipped; this remains the substrate cross-type preload (above) would build on, deferred until that driver appears.

## Rate-limit unification

GitHub fetches objects via the endpoint path (no rate-limit detection) while structural paths are rate-limit-aware, so the two HTTP paths handle 429s differently. The fix is a rate-limit-aware endpoint terminal that unifies them on the existing breaker (`crates/omnifs-sdk/src/rate_limit.rs`), so object reads get the same backoff structural reads already have.

## Explicit pagination control namespace

`@pages` was a sketched explicit-pagination control namespace, parked in favour of listing honesty plus `@next` / `@all`. It is recorded here as a deliberately-deferred idea, not an active plan; revisit only if the control-file model proves insufficient.

## Archive world convergence

`omnifs:tool-archive` is a separate WIT world from `omnifs:provider`. Whether the archive extraction tool should converge with the provider world (one contract, one sandbox model) rather than stay a sibling is an open structural question, low priority while the two surfaces are stable.

## Native mount frontends

The detailed design for additional frontends (NFSv4 loopback, FSKit on macOS) and the distributed frontend-runtime split lives in the internal design notes; the architecture already keeps FUSE as one frontend of the projected tree behind the `omnifs_host::Namespace` seam, so no provider-facing API depends on the frontend. This is product-gated, not technically blocked.
