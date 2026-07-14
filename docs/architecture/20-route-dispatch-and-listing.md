# Route dispatch and listing

Status: current-architecture
Scope: why SDK route dispatch separates lookup, listing, read, and open while keeping one precedence model. Binding rules live in `docs/contracts/20-provider-sdk.md` and `docs/contracts/30-projection-tree.md`.

`omnifs` is navigated incrementally. A shell may `cd`, `ls`, `stat`, and then read. The dispatcher must answer whether a concrete path is owned, whether it is enumerable, and whether absence from a prior listing is authoritative.

## Route precedence

Route matching follows a most-specific-wins model:

1. non-rest patterns beat rest patterns
2. more literal segments beat fewer literal segments
3. prefix captures beat bare captures
4. longer patterns break ties

Ambiguous routes that could match the same concrete path with the same precedence must fail when the registration builder is compiled. `Router::compile` consumes the mutable builder and returns the only runtime-dispatchable form, `CompiledRouter`. A route tree that cannot compile is an invalid provider component and is never published by provider initialization.

## Capture validation

Capture parsers participate in match candidacy. A candidate route whose captures fail to parse does not own the path, and dispatch falls through to the next candidate.

This is what lets providers model adjacent typed paths without read-time hacks. For example, one route can accept an IP-shaped segment while a sibling route rejects it and accepts a domain-shaped segment.

## Lookup and listing authority

`lookup(parent, name)` is the authoritative name oracle for a specific child.

`readdir(parent)` is an enumeration of what the provider chose or was able to list at that time. It may be non-exhaustive.

Absence from a non-exhaustive listing is not ENOENT. A valid child can be reachable by lookup even if it did not appear in a previous directory listing.

The converse holds too: presence in an already-served listing must never regress to ENOENT. A host pagination control (`@next`/`@all`) a consumer resolved from an earlier listing snapshot keeps resolving, and its read stays a no-op, even after the feed exhausts and a fresh listing stops naming the control.

## Exhaustive listings

A listing is exhaustive only when every child name currently knowable by that route surface was enumerated.

A hard cap without a resume cursor is non-exhaustive. A real resume cursor can be exposed through host pagination controls. Fake cursors and exhaustive claims over truncated data are bugs.

Auto-derived literal directories can be exhaustive only when no dynamic capture sibling exists at the next depth.

## Auto-navigable directories

Literal route prefixes are auto-navigable. If a provider registers `/categories/{category}/papers`, then `/categories` can exist as a navigation directory without a stub handler.

Capture prefixes are not auto-navigable by themselves. A capture segment ranges over an unbounded keyspace, and only the provider's handler can decide which concrete names exist.

This distinction avoids forcing provider authors to write empty literal scaffolding while still keeping dynamic namespaces honest.

## Static and dynamic children

A directory listing merges literal route-table siblings with provider-enumerated children. Capture siblings contribute resolvability, not enumerable names.

Do not duplicate static sibling merge logic across list and lookup paths. The route dispatcher owns it once.

## Negative results

Negative lookup is authoritative only when no route candidate, explicit child, dynamic capture sibling, or parent handler can own the child.

Negative cache policy must be shared by the tree or host layer. Frontends should not add local dotfile exceptions, lookup suppression lists, or provider-specific negative heuristics.

## Provider authoring guidance

Use explicit directory handlers where children are capture-routed. That handler is the source of truth for concrete child names and lookup verdicts.

Do not write stub handlers for literal-only navigation nodes. Let the router synthesize those.

Use captures to model parsed domain values. Do not pass raw strings inward after a parse boundary when a typed capture can reject bad segments.

## Rejected shapes

- operation-specific dispatch ordering
- fake exhaustive listings over capped data
- static route scaffolding that binds as a dynamic capture
- prefix deletion or prefix lookup where exact route ownership is required
- host or frontend provider-specific route behavior
