# Route dispatch and listing

Status: current-architecture
Scope: why SDK route dispatch separates lookup, listing, read, and open while
keeping one precedence model.

Read when: changing route registration, capture matching, implicit
directories, lookup, listing, pagination, or listing exhaustiveness.

Binding contracts: `docs/contracts/20-provider-sdk.md` and
`docs/contracts/30-projection-tree.md`.

Filesystem traversal is incremental. The dispatcher decides path ownership,
enumerability, and whether an absent listing entry proves absence.

## Route precedence

Route matching follows a most-specific-wins model:

1. non-rest patterns beat rest patterns
2. more literal segments beat fewer literal segments
3. prefix captures beat bare captures
4. longer patterns break ties

`Router::compile` consumes the mutable builder, freezes mounted aliases,
resolves collections, rejects equal-precedence ambiguity, and returns the only
runtime form, `CompiledRouter`. Initialization never publishes an invalid
route tree.

## Capture validation

Capture parsers participate in candidacy. A parse failure rejects that
candidate and dispatch falls through.

`#[path_captures]` fields are required by default. An `Option<T>` segment may
be absent when related routes reuse the key type; when present, it still uses
`T`'s parser.

This supports adjacent typed paths, such as IP and domain captures, without
read-time branching.

## Lookup and listing authority

`lookup(parent, name)` is authoritative for one child. `readdir(parent)` lists
what was knowable at that time and may be partial. Absence from a partial
listing is not `NotFound`.

An entry already served by a listing cannot regress to `NotFound`. A resolved
pagination control (`@next` or `@all`) remains a no-op even after a fresh
listing stops naming it.

## Exhaustive listings

A listing is exhaustive only when it enumerates every currently knowable child.
A cap without a real resume cursor is partial. A literal-only route directory
is complete only when no next-depth capture can own more names.

## Auto-navigable directories

Literal prefixes are auto-navigable. Registering
`/categories/{category}/papers` creates `/categories` without a stub handler.

Capture prefixes are not auto-navigable: only the provider can enumerate their
unbounded keyspace.

This removes empty literal scaffolding while keeping dynamic namespaces honest.

## Static and dynamic children

A directory merges literal route siblings with provider-enumerated children.
Captures add resolvability, not names. The dispatcher owns this merge for both
list and lookup.

## Negative results

Negative lookup is authoritative only when no route, explicit child, capture,
or parent handler can own the name. Shared tree policy owns negative caching;
filesystems add no exceptions or suppression rules.

## Provider authoring guidance

- Use an explicit directory handler for capture-routed children; it owns names
  and lookup verdicts.
- Let the router synthesize literal-only navigation nodes.
- Use typed captures to reject bad segments at the parse boundary.

## Rejected shapes

- operation-specific dispatch ordering
- fake exhaustive listings over capped data
- static route scaffolding that binds as a dynamic capture
- prefix deletion or prefix lookup where exact route ownership is required
- host or filesystem provider-specific route behavior
