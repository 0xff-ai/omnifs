# Provider rewrite design

Status: implemented in this branch

## Purpose

The provider tree is being reset against the current SDK instead of preserved as
an incremental migration artifact. The goal is not fewer lines by itself. The
goal is that every provider reads as a direct implementation of its domain
surface:

- object-oriented providers bind canonical upstream resources to object routes
  and put derived leaves on the object;
- path-oriented providers keep direct file and directory handlers when there is
  no stable canonical object behind the path;
- route topology remains visible in `start()`;
- provider-local caches, compatibility bridges, and
  repeated projection helpers disappear unless they own a real domain rule;
- tests protect provider path contracts and SDK/host effects, not helper names.

## Provider classification

### arXiv

Flavor: hybrid, mostly object-oriented.

The canonical object is a paper Atom feed. Category browsing and blob downloads
are path-oriented, but every paper artifact must live under an explicit version
selector. The paper route is a version family:

```text
/papers/{paper}/@latest/{paper.atom,paper.json,paper.pdf,source.tar.gz}
/papers/{paper}/vN/{paper.atom,paper.json,paper.pdf,source.tar.gz}
/categories/{category}/papers/{paper}/@latest/...
/categories/{category}/papers/{paper}/vN/...
```

`@latest` is dynamic. Numbered versions are stable. The version selector is
a route facet, not part of paper identity. Direct paper leaves and the old
`versions/` directory are removed.

### DB

Flavor: path-oriented.

The database file is already the local source of truth, so the provider must not
emit canonical object-cache entries for database metadata or table metadata.
`/meta/info.json`, `/meta/version.txt`, `/meta/path.txt`,
`/tables/{table}/table.json`, and the table field leaves are direct reads from
the SQLite backend. `sample.json` remains direct and may use ranged reads for
large samples.

The table name parser admits only names observed at provider start. That is a
current SDK routing compromise, not a cache: without it, dynamic table routes can
synthesize `/tables/{missing}` as a navigable anchor before any DB handler runs.
The table universe is a local read-only snapshot for the mount, so the startup
admissibility set preserves real lookup semantics without involving the host
object cache.

### DNS

Flavor: path-oriented.

DNS answers are query results, not durable objects. Domain and resolver captures
are validated path segments, and record files are dynamic query leaves. The
provider should stay direct: routes call resolver policy, resolver policy owns
default versus named resolver selection, and there is no fake DNS object cache.

### Docker

Flavor: path-oriented.

The Docker daemon is operational state, not a stable canonical object source.
System endpoints, list files, and container leaves are fresh path reads. The
same direct handlers are mounted under by-name, by-id, running, stopped, and
Compose service aliases. Docker must not emit canonical object-cache entries.

### GitHub

Flavor: object-oriented with one subtree handoff.

Repository, issue, pull request, and workflow run are SDK objects. Lists are
path-oriented discovery routes that preload available object leaves when the
upstream payload already contains them. Body-derived leaves remain deferred when
list payloads do not carry the full body contract. `repo` remains a `TreeRef`
handoff: provider dispatch stops at producing the tree reference.

### Linear

Flavor: object-oriented.

Teams and issue filters are path-oriented discovery routes. Issues are objects.
List payloads already contain enough fields for shallow leaves, so list routes
may project those leaves eagerly. The issue identity is the Linear identifier;
team and filter captures are facets that validate alias context.

### Test provider

Flavor: SDK conformance fixture.

The test provider is not a product provider. It intentionally exercises SDK
features such as scoped invalidation, deferred reads, ranged files, paged
directories, and subtree handoff. It should not be rewritten to match product
provider aesthetics.

## Cross-provider rules

### Route topology

Each provider keeps the main path tree in its `start()` function. It is fine to
construct reusable object handles, but the final route surface must be readable
without jumping through a registration DSL hidden in a helper module.

### Path captures

Path newtypes validate syntax only unless their allowed values are truly static
or the current SDK would otherwise synthesize a false dynamic anchor before a
handler can run. Existence is normally decided by listing handlers, lookup
intent, or object `Key::load`. DB is the exception because its table universe is
known from a local read-only snapshot at provider start.

### Objects

Use `r.object::<O>(...)` when a path has a canonical upstream payload and
multiple derived leaves. The object owns field projection methods. Repeated
`load -> derive field -> build projection` handlers are a smell unless the
leaf needs route-specific behavior that the object projection API cannot
express.

### Path handlers

Use `r.dir`, `r.file`, and `r.treeref` when the path is a direct operation:
query DNS, list Docker containers, fetch a PDF blob, stream a log, open a git
tree, or read a configured local database sample. These handlers should have
typed keys and should keep policy on the type or API adapter that owns it.

### Preloading

If a list payload already contains fields that the user can read immediately,
the provider should emit those derived leaves in the same response. If the list
payload does not contain a trustworthy full field, emit a deferred file with the
right attributes instead of pretending the list row is canonical.

### Stability

Dynamic upstream resources are dynamic projections. Versioned upstream
resources are stable projections. Local database snapshots use backend
validators when available. Live files require ranged reads.

### Errors

Parse rejection is for impossible path syntax. `NotFound` is for absent upstream
resources or absent backend rows. Provider internals should attach operation
context to internal errors, but should not wrap one obvious call in a named
helper only to rename the error.

## Implementation checklist

### arXiv

- [x] Replace direct paper leaves with version selector directories.
- [x] Add `@latest` and `vN` path capture support.
- [x] Keep version as a facet, not canonical paper identity.
- [x] Make the paper root list `@latest` plus numbered versions from Atom.
- [x] Mark `@latest` leaves dynamic and numbered version leaves stable.
- [x] Remove provider-local paper cache.
- [x] Reject old direct paper leaves and old `versions/` paths in tests.

### DB

- [x] Remove SDK object registration from DB.
- [x] Keep DB out of the canonical object cache.
- [x] Keep `/meta` as direct directory and file handlers.
- [x] Keep `/tables/{table}` as direct directory and file handlers.
- [x] Keep table admissibility at provider start to prevent synthetic missing
      table anchors.
- [x] Keep `sample.json` direct because it depends on configured limits and may
      switch to ranged reads.

### DNS

- [x] Keep DNS path-oriented.
- [x] Keep resolver selection inside resolver policy.
- [x] Avoid introducing fake DNS objects or caches.
- [x] Preserve default resolver, named resolver, and reverse lookup paths.

### Docker

- [x] Keep Docker path-oriented.
- [x] Keep system and listing endpoints path-oriented.
- [x] Keep container leaves fresh and free of canonical object-cache effects.
- [x] Preserve by-name, by-id, running, stopped, and Compose alias surfaces.
- [x] Keep Compose project/service discovery path-oriented.

### GitHub

- [x] Keep repository, issue, pull, and workflow run as objects.
- [x] Keep lists path-oriented and eager-project only fields present in list
      payloads.
- [x] Keep body, item markdown, item JSON, and pull diff deferred where the
      list payload is not the full leaf contract.
- [x] Keep `repo` as subtree handoff.
- [x] Remove duplicated list-preload conversion glue.

### Linear

- [x] Keep teams and filters as path-oriented discovery.
- [x] Keep issues as objects.
- [x] Project list-derived shallow issue leaves from the payload.
- [x] Remove duplicated list-preload conversion glue.

### Validation

- [x] `cargo fmt --check`
- [x] provider WASM build for touched providers
- [x] targeted provider clippy/check for touched providers
- [x] provider integration tests for changed public surfaces
