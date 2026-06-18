# Path dispatch and listing semantics

Status: superseded by docs/design/object-cache-primary.md and docs/design/architecture.md §6 for current dispatch semantics; retained as the rules-of-dispatch reference for provider authors.
Scope: `crates/omnifs-sdk` (route registration, lookup/list dispatch), `crates/omnifs-host` (FUSE cache, lookup short-circuit), provider authoring conventions

## Context

omnifs is a navigable virtual filesystem. Providers register typed route patterns; the host walks paths interactively (`cd`, `ls`, `stat`) and the kernel issues `lookup`, `readdir`, and `read` operations through FUSE. The dispatch layer must answer three questions for every concrete path it sees:

1. Does any registered route own this path?
2. If the path is a directory, what are its children?
3. If a child name is missing from the listing, is its absence authoritative or open?

Two closely-related domains have established practice we draw from. URL routers (Express, Django, Rails, ASP.NET endpoint routing, Spring `PathPatternParser`, Phoenix, Next.js App Router, Go `httprouter`) match a complete path once per request and converge on a specificity-based precedence ladder. Virtual filesystems (Linux `/proc` and `/sys`, plan9 9P, FUSE-backed namespaces) are walked interactively and split `lookup` from `readdir` as separate authority structures.

omnifs's dispatch combines both: routes are typed and prioritized like a URL router; navigation is incremental and `lookup` may resolve names that `readdir` did not enumerate. The rules below formalize that combination.

## Decisions

### D1. Specificity-based precedence

Patterns are ordered by `Pattern::precedence_key()`, a 4-tuple compared lexicographically:

1. Non-rest patterns sort above rest patterns. Rest captures are catch-alls; narrower patterns must win where they overlap.
2. More literal segments wins.
3. More prefix captures (e.g. `_v{ver}`) wins over bare captures.
4. Longer patterns win as a tiebreaker.

Counts are summed over the pattern, not compared segment-by-segment from the left. Pairs that could match the same concrete path with the same precedence key are rejected at registration by `Router::seal`, which uses the `Pattern::is_ambiguous_with` predicate; this is the safety valve that makes count-based precedence sound.

### D2. Per-segment validators participate in match candidacy

A capture's parse function is part of selection, not a post-match check. When two routes' patterns shape-match the same path, the dispatcher tries them in precedence order and returns the first whose parse function accepts the path. A parse rejection means "this candidate does not own this path" and the dispatcher falls through to the next-most-specific candidate. This mirrors Rails constraints, Spring `PathPattern`, and httprouter typed routes.

### D3. `lookup` is the authoritative name oracle; `readdir` is allowed to be non-exhaustive

This is the procfs / 9P / FUSE invariant. A directory listing carries an `exhaustive` flag whose meaning is **"the names I am enumerating are the names I am aware of"**, not **"no other paths exist"**. `lookup(parent, name)` may return a positive result even when `name` did not appear in the most recent `readdir` of `parent`. The inverse — concluding ENOENT from absence in a listing — is constrained by D4.

### D4. Negative `lookup` is authoritative only when no dynamic-capture sibling could match

`lookup(parent, name)` resolves to ENOENT only when both hold:

- `name` is absent from the parent's enumerated set, AND
- no `/{capture}` or `/{*rest}` sibling route's parse function accepts `name`.

Otherwise the dispatcher invokes the matching capture handler and returns its verdict. The host owns this decision because only the host has both the cached listing and the resolved manifest; providers see one path at a time and cannot reason about siblings.

### D5. Intermediate directories are auto-navigable along literal-segment prefixes

omnifs is interactive. A user types `cd /categories`, then `ls`, then `cd cs.AI`. Intermediate paths must be addressable as directories without forcing the provider to register a stub `r.dir(...).handler(...)` for every literal navigation node.

The rule has two halves:

- **Literal-segment prefix → auto-navigable.** A path resolves as a directory if it has no explicit handler but its last segment appears as a literal child of its parent in the route table (i.e. some registered route has a literal at the corresponding depth, with a matching prefix above). Listing such a directory merges sibling-route literal children. The `exhaustive` flag is true iff no `Capture` or `Rest` segment appears at the corresponding depth in any registered route.
- **Capture-segment prefix → not auto-navigable.** A capture segment ranges over an unbounded keyspace; the SDK has no way to validate which concrete values inhabit it without the per-route parse function (D2), and the parse function takes a complete concrete path, not a prefix. Paths whose ancestry goes through a capture must resolve through `match_dir` directly. Their parent directory must have an explicit `r.dir(...).handler(...)` registration whose projection is the source of truth for which children exist.

This is the load-bearing distinction from URL routers, which do not auto-derive intermediates because there is no `cd` in HTTP. It is also the load-bearing distinction from "every directory is a static enumeration": capture children are discovered by the provider, not by the route table.

### D6. `readdir` merges static shape (literal-named siblings) with the parent dir handler's enumeration

Capture-route siblings are unbounded and cannot enumerate. They contribute to the resolvable set (D4), not the enumerable set. This matches Next.js's rule that catch-all segments do not materialize URLs at build time without an explicit generator.

### D7. A listing's `exhaustive` flag combines provider claim and route-table fact

For an explicit dir handler, `exhaustive` is whatever the provider returned. For an auto-navigable directory (D5), `exhaustive` is computed: true iff no dynamic-capture sibling exists at depth + 1.

A provider can still claim `exhaustive: true` on a directory whose siblings include capture routes — that is a provider authorship error analogous to declaring a wrong content-type. D4 is the safety net that prevents such a claim from becoming a permanent ENOENT for valid capture-routed children.

### D8. Compute precedence and route-table predicates at registration

Spring `PathPatternParser`, ASP.NET endpoint graph, and httprouter's radix tree all bake ordering into a structure built once. omnifs's `Router::seal` (called by the provider macro after `start`) is the natural place to sort and freeze. Per-request route walks are acceptable while route counts remain small; precomputed structures are the path forward as the registry grows.

## Pattern grammar

A `Pattern` is an ordered sequence of segments. Each segment is one of:

| Segment        | Syntax       | Matches                                                             | Validator                  |
|----------------|--------------|---------------------------------------------------------------------|----------------------------|
| Literal        | `users`      | exactly the literal string                                          | none                       |
| Bare capture   | `{name}`     | any single non-empty path segment                                   | route's parse function     |
| Prefix capture | `_v{ver}`    | any segment with the static prefix and a non-empty suffix           | route's parse function     |
| Rest capture   | `{*tail}`    | zero or more trailing segments, joined by `/`                       | route's parse function     |

Constraints (enforced by `Pattern::parse`):

- A rest segment is allowed only as the last segment of a pattern.
- A pattern has at most one rest segment.
- Capture names are non-empty identifiers; prefix captures require a non-empty static prefix.
- Bare and prefix captures match exactly one path segment; only rest captures span multiple.

## Match algorithm

The single matching primitive is `best_match(routes, path)` in `crates/omnifs-sdk/src/router/pattern.rs`, used by `Router` dispatch for dirs, files, treerefs, and objects:

1. Filter routes to those whose pattern shape accepts `path` (`Pattern::matches_path`).
2. Sort the remaining candidates by `precedence_key` descending.
3. Return the first candidate whose parse function accepts `path`.

Step 3 implements D2's fallthrough. A candidate that pattern-shape-matches but whose parse rejects is not a winner; the next candidate is tried.

## Listing semantics

`Router::list_children(path)` resolves in this order:

1. Subtree handler at `path` → return subtree handoff.
2. Explicit dir handler at `path` → invoke handler, return its `Listing` merged with literal sibling children. The `exhaustive` flag is the provider's claim.
3. Auto-navigable directory at `path` (D5) → synthesize a `Listing` of literal sibling children. `exhaustive` per D7.
4. File handler at `path` → return `not_a_directory` error.
5. Otherwise → return `not_found`.

`Router::lookup_child(parent, name)` resolves in this order, where `child = parent + "/" + name`:

1. Subtree handler at `child` → return subtree handoff.
2. Explicit dir handler at `child` (and not also a file handler — the dir+file co-existence case) → invoke handler with `DirIntent::List` to warm the child's adjacent shape, return entry.
3. Auto-navigable directory at `child` → return dir entry with literal siblings.
4. Explicit file or subtree exact handler at `child` → return file/dir entry as appropriate.
5. Otherwise: invoke parent's explicit dir handler with `DirIntent::Lookup { child: name }`. The handler's projection authoritatively decides whether `name` exists.
6. If parent has no explicit dir handler: return `not_found`. (Auto-navigable parents only resolve children that are themselves auto-navigable or exact-handler matches; capture-routed children require the parent to have a real handler.)

This resolution order honors D2, D4, and D5: capture validation runs in steps 1, 2, 4, 5 via `match_*`; auto-derivation only adds literal-prefix paths (step 3); and the parent dir handler is consulted before any negative conclusion is reached.

## Cross-kind coexistence

A single template may be registered as both a dir and a file handler (`r.dir(t).handler(h)` and `r.file(t).handler(h)`) — the dispatcher routes by request kind (`list_children` → dir, `read_file` → file). Subtree handlers are mutually exclusive with dir/file on the same template, since a subtree takes the path entirely. `Router::seal` enforces this at registration.

When dir and file co-exist on a template (always rest-captured, by current validator rules), `lookup_child` defers to the parent dir's projection verdict to disambiguate the child's kind.

## Cross-mount

The host's mount table is a flat name → mount lookup. Mount names are unique; mount selection is by exact match of the path's leading segment under the FUSE root. There is no cross-mount precedence question.

## Provider authoring guidance

- **Don't write stub handlers for literal navigation nodes.** `cd /categories` works without an `r.dir("/categories").handler(...)` registration if any registered route has `categories` as a literal at depth 1. Add an explicit handler only when the directory has data to project.
- **Do write explicit handlers at directories whose children are capture-routed.** A directory whose children are matched by `/{capture}` cannot be auto-navigated through; the parent handler is the source of truth for which concrete children exist (and informs `lookup` of names not in the enumeration through D4).
- **Don't claim `exhaustive: true` if your directory has capture siblings in the route table.** D7 documents the convention. D4 will recover from the mistake at lookup time, but the listing wire-data is still inaccurate.
- **Use parse functions to validate captures.** A `{domain}` segment is enforced by its parse function rejecting raw IPs; a `{ip}` segment by its parse function accepting them. Two capture-shape siblings with disjoint validators (D2 fallthrough) are a clean way to model heterogeneous typed paths.

## Host invariants

- The mount root carries a host-synthesized `AGENTS.md` leaf appended at the shared `Namespace` seam (`crates/omnifs-host/src/{agents_doc,namespace}.rs`). It is injected only at the root, only into a concrete listing, and only when the provider did not already enumerate or resolve `/AGENTS.md` (provider-first; a provider's own `/AGENTS.md` wins and the listing never duplicates the name). Its advertised size equals the bytes `read_file` returns, so it behaves like a real `text/markdown` file for `stat`/`wc -c`/`cat`.
- The negative-cache short-circuit in `lookup_check_caches` reads the parent's cached `Dirents` only when `dirents.exhaustive` is true. The exhaustive flag is the SDK's responsibility to compute correctly per D7.
- The lookup-side `cache_projection_batch` writes a `Dirents` record only when the lookup carried child information in the direct result. A bare lookup entry is not a directory listing and must not synthesize one.

## Sources

- ASP.NET Core endpoint routing — precedence ladder, most-specific-wins.
- Spring `PathPatternParser` — specificity precedence, typed-capture-as-candidacy.
- Next.js App Router — static > dynamic > catch-all; catch-all does not auto-materialize URLs.
- Rails / Django — regex/converter constraints as gating predicates with fallthrough.
- Linux `/proc` — `proc_root_readdir` enumerates current PIDs; `lookup` resolves any live PID independent of recent enumeration.
- 9P protocol — `walk` (per-segment lookup) is independent of `read` on a directory fid.
- libfuse — `entry_timeout` and negative-lookup timeout separately tunable; `readdir` may legitimately omit live entries.
