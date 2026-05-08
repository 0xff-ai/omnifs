# Writing a provider

**Read when:** authoring or modifying a provider, touching the
`omnifs-sdk` / `omnifs-sdk-macros` crates, changing the WIT, or working on
host-side dispatch. Also read if a path isn't resolving the way you expect.

**Update when:** adding, renaming, or removing a handler attribute or
top-level macro; changing the WIT contract; changing the browse-method
surface (`lookup_child` / `list_children` / `read_file` / streaming);
changing dispatch precedence, auto-navigability, or exhaustiveness rules
(also update `docs/design/path-dispatch-and-listing.md`); or changing the
callout/resume protocol shape.

For routing/listing rules in detail, this file is a summary; the source of
truth is `docs/design/path-dispatch-and-listing.md`. **Read that doc before
changing dispatch logic.**

## Provider shape

Each provider is a WASM component (`wasm32-wasip2`) implementing the
`omnifs:provider` WIT interface (`wit/provider.wit`). Providers are
authored as **free-function path handlers** collected from
`#[omnifs_sdk::handlers] impl ...` blocks, with the provider's mounts
declared via `#[omnifs_sdk::provider(mounts(...))]`.

## Handler attributes (inside `#[handlers]`)

| Attribute            | Meaning                                                    |
|----------------------|------------------------------------------------------------|
| `#[dir("...")]`      | Directory path family                                       |
| `#[file("...")]`     | Exact file path family                                      |
| `#[treeref("...")]`  | Subtree handoff: returns a `TreeRef` that the host resolves to a bind-mounted clone directory |
| `#[bind("...")]`     | Mounts a typed subtree (`#[subtree] impl B { ... }`) at this path family. The host parses prefix captures, constructs `B`, and dispatches the suffix through `B`'s inner registry |
| `#[mutate("...")]`   | Mutation handler                                            |

## Top-level attributes

| Attribute                                  | Purpose                                |
|--------------------------------------------|----------------------------------------|
| `#[omnifs_sdk::config]`                     | Provider config struct                  |
| `#[omnifs_sdk::subtree] impl B { ... }`     | Typed-subtree-dispatch impl block whose inner `#[dir]` / `#[file]` items are templates relative to the subtree root |
| `#[omnifs_sdk::provider(mounts(...))]`      | Provider entrypoint and mount list      |

Note: `#[treeref]` (formerly `#[subtree]`) is the SDK-side attribute for
subtree handoff so that `#[subtree]` is free for typed-subtree dispatch.
The WIT result variant is still spelled `subtree`.

## Host browse surface

- `lookup_child(id, parent_path, name)` — resolves one child entry.
- `list_children(id, path)` — lists a directory.
- `read_file(id, path)` — reads exact file content.

Subtree handoff folds into `lookup_child` and `list_children`: when a
`#[treeref]` handler matches, the corresponding
`lookup-result::subtree(tree-ref)` / `list-result::subtree(tree-ref)`
terminal is returned and the host resolves the handle to a bind-mounted
clone directory.

## Browse model summary

- Any registered route's literal-segment prefix is an auto-navigable
  directory; **do not write no-op stub `#[dir]` handlers** for intermediate
  nodes (`/categories` is implicit when `/categories/{category}` exists).
- Per-segment validators (route parse functions) participate in match
  candidacy. A parse rejection falls through to the next-most-specific
  candidate, not to ENOENT.
- `lookup_child` answers subtree handlers first, then exact/static/auto-
  navigable shape, then the parent `#[dir]` for dynamic children.
- `list_children` answers subtree handlers first, then merges static child
  shape with the parent directory projection. An auto-navigable
  directory's listing is `exhaustive=false` whenever any sibling route at
  depth+1 has a capture or rest segment.
- `read_file` uses exact `#[file]` handlers first, then allows bounded
  eager bytes from a parent directory projection for projected files.

## Callouts and resume

Providers return either:

- a terminal `op-result` (wrapped in `provider-return`), or
- suspend with a list of `callout`s.

The host executes the callout batch and calls `resume(id, results)` with
the outcomes. Providers store continuations keyed by correlation ID to
resume from where they left off.

Callouts are strictly request/response; **there are no fire-and-forget
callouts**. Cache side effects ride inside the terminal — see
`.rules/caching.md`.

## Always project everything you've already paid for

If a handler has fetched an upstream payload that contains data for sibling
files or nested children, return it. The relevant attachments:

- `FileContent::with_sibling_files(..)` on `read` routes
- `Lookup::with_sibling_files(..)` on `lookup` routes
- `Projection::preload` / `preload_many` for content that isn't a direct
  sibling but is nested under the listed/looked-up entry

This is non-negotiable: forcing the host into a second round trip when the
bytes are in hand defeats the architecture. Failing to project known data
is treated as a code-review blocker.

## Streaming surfaces

The WIT reserves `open-file` / `read-chunk` / `close-file` for streamed and
ranged file reads, but the current host/runtime path serves exact file
bytes plus explicit subtree handoff. Don't hook new code to the streaming
arms unless you're implementing the streaming path.

## Configs are JSON

Instance configs are JSON, not TOML. The host parses each mount's JSON
config into an `InstanceConfig`; the provider-specific `"config"` object is
preserved as a `serde_json::Value` and re-serialized to JSON bytes for the
`initialize()` call. Providers receive the raw payload as JSON bytes and
deserialize via `serde_json::from_slice` (the SDK's `#[config]` macro
wires this up automatically).
