# Provider protocol: callouts, returns, and effects

Scope: `crates/omnifs-wit/wit/provider.wit`, host runtime, SDK, SDK macros, providers.

## Vocabulary

The protocol gives each semantic channel one name and one place in the
WIT:

- `callout`: intermediate work the provider asks the host to run, then
  receives through `continuation.resume`.
- `return`: the completed operation answer.
- `effect`: a host-side mutation committed because a return was accepted.

The load-bearing sentence: a provider step either suspends on callouts
or returns a result with effects.

## WIT shape

Types not redefined in this block are kept from the current
`crates/omnifs-wit/wit/provider.wit` unless a later phase explicitly changes them.

```wit
type correlation-id = u64;
type tree-ref = u64;
type file-handle = u64;
type blob-id = u64;

/// One provider progress step. `suspended` is not a return; the host must
/// run a non-empty callout batch and resume the same operation. `returned` is
/// the completed operation answer plus host-side effects to commit.
variant provider-step {
    suspended(list<callout>),
    returned(provider-return),
}

/// Work the host runs on the provider's behalf. Every callout expects a
/// matching `callout-result` back via `resume`; there are no fire-and-forget
/// callouts, and a provider return can never carry trailing callouts.
variant callout {
    fetch(http-request),
    git-open-repo(git-open-request),
    fetch-blob(blob-fetch-request),
    open-archive(archive-open-request),
    read-blob(read-blob-request),
}

variant callout-result {
    http-response(http-response),
    git-repo-opened(git-repo-info),
    blob-fetched(blob-fetched),
    archive-opened(archive-opened),
    blob-read(list<u8>),
    callout-error(callout-error),
}

type callout-results = list<callout-result>;

type version-token = string;

variant file-size {
    exact(u64),
    non-zero,
    unknown,
}

enum read-mode {
    full,
    ranged,
}

enum stability {
    immutable,
    mutable,
    volatile,
}

/// File metadata. Bytes are deliberately not attributes.
record file-attrs {
    size: file-size,
    stability: stability,
    version-token: option<version-token>,
}

/// Byte availability declared by projection.
variant proj-bytes {
    inline(list<u8>),
    deferred(read-mode),
}

/// File projection data: metadata plus byte availability.
record file-proj {
    attrs: file-attrs,
    bytes: proj-bytes,
}

variant entry-kind {
    directory,
    file(file-proj),
}

record dir-entry {
    name: string,
    /// A direct result entry may include `proj-bytes::inline(...)`. Those
    /// bytes are for this entry only and are committed with the accepted
    /// lookup or listing result. Use `effects.fs` for additional paths.
    kind: entry-kind,
}

/// Completed operation answer. This shape cannot carry callouts.
/// `effects` is the only terminal host-mutation channel in the target WIT.
record provider-return {
    result: op-result,
    effects: effects,
}

/// Host-side mutations committed at the return boundary.
record effects {
    /// Object-cache writes: anchor → { version, bytes, leaves }.
    canonical: list<canonical-store>,
    /// View-cache writes: path metadata or dirents to install.
    fs: list<fs-write>,
    /// Invalidations to apply after the return is accepted.
    invalidations: list<invalidation>,
}

record canonical-store {
    anchor: string,
    version: option<version-token>,
    bytes: list<u8>,
    /// Anchor-relative leaf remainders, e.g. ["/item.md", "/title"] or [".md", ".json", ".raw"].
    leaves: list<string>,
}

/// Provider-relative path metadata to install into the host view cache.
record fs-write {
    path: string,
    kind: entry-kind,
}

/// Lookup-child result. `subtree(tree-ref)` answers the lookup/list operation.
/// It is result data, not itself the host mutation.
variant lookup-child-result {
    entry(lookup-entry),
    subtree(tree-ref),
    not-found,
}

record lookup-entry {
    target: dir-entry,
    /// Other children of the same parent learned while resolving `target`.
    /// This intentionally stays separate from `dir-listing`: the lookup
    /// result has one primary target plus sibling hints.
    siblings: list<dir-entry>,
    /// If true, `target + siblings` is the complete child set for the parent.
    /// Absence from that set is a sound negative lookup. If false, siblings
    /// are cache hints only and must not be used to deny lookup.
    exhaustive: bool,
}

variant list-children-result {
    entries(dir-listing),
    subtree(tree-ref),
}

record dir-listing {
    entries: list<dir-entry>,
    exhaustive: bool,
}

/// Materialized byte source returned by `read-file`.
variant read-file-bytes {
    inline(list<u8>),
    /// Opaque host blob handle previously returned by `fetch-blob` to this
    /// provider runtime. In the current WIT, `fetch-blob` is the only
    /// callout that produces a `blob-id`; `open-archive` consumes one and
    /// returns a `tree-ref`, and `read-blob` consumes one and returns bytes.
    blob(blob-id),
}

/// Completed full-read result. `read-file` is only valid for files projected
/// with `proj-bytes::deferred(read-mode::full)`.
record read-file-result {
    attrs: file-attrs,
    bytes: read-file-bytes,
}

/// Opened ranged-read handle. `open-file` is only valid for files projected
/// with `proj-bytes::deferred(read-mode::ranged)`.
record open-file-result {
    handle: file-handle,
    attrs: file-attrs,
}

/// Chunk bytes for an `open-file` handle.
record read-chunk-result {
    content: list<u8>,
    eof: bool,
}

record initialize-result {
    info: provider-info,
}

record plan-mutations-result {
    mutations: list<planned-mutation>,
}

record execute-result {
    outcome: mutation-outcome,
}

record fetch-resource-result {
    files: list<file-entry>,
}

/// Completed operation answer. Arm names mirror provider operations; `error`
/// is the cross-operation failure arm.
variant op-result {
    lookup-child(lookup-child-result),
    list-children(list-children-result),
    read-file(read-file-result),
    open-file(open-file-result),
    read-chunk(read-chunk-result),
    initialize(initialize-result),
    on-event,
    /// Cross-operation failure. Do not add `error-result`: future failure
    /// modes extend `provider-error`.
    error(provider-error),
    plan-mutations(plan-mutations-result),
    execute(execute-result),
    fetch-resource(fetch-resource-result),
}

interface lifecycle {
    /// Terminal-only in this iteration. It has no correlation id, so it cannot
    /// suspend on callouts unless the interface is explicitly changed later.
    initialize: func(config: list<u8>) -> provider-return;
    shutdown: func();
    get-config-schema: func() -> option<string>;
    capabilities: func() -> requested-capabilities;
}

interface namespace {
    lookup-child: func(id: correlation-id, parent-path: string, name: string)
        -> provider-step;
    list-children: func(id: correlation-id, path: string) -> provider-step;
    read-file: func(id: correlation-id, path: string) -> provider-step;
    open-file: func(id: correlation-id, path: string) -> provider-step;
    read-chunk: func(
        id: correlation-id,
        handle: file-handle,
        offset: u64,
        len: u32,
    ) -> provider-step;
    /// Fire-and-forget handle cleanup. It cannot suspend, return, or carry
    /// effects.
    close-file: func(handle: file-handle);
}

interface reconcile {
    plan-mutations: func(id: correlation-id, changes: list<file-change>)
        -> provider-step;
    execute: func(id: correlation-id, mutation: planned-mutation)
        -> provider-step;
    fetch-resource: func(id: correlation-id, resource-path: string)
        -> provider-step;
}

interface notify {
    on-event: func(id: correlation-id, event: provider-event)
        -> provider-step;
}

interface continuation {
    resume: func(id: correlation-id, results: callout-results) -> provider-step;
    /// Fire-and-forget cancellation. It cannot suspend, return, or carry
    /// effects.
    cancel: func(id: correlation-id);
}
```

## Protocol invariants

- A suspended step must contain at least one callout.
- A returned step never carries callouts, trailing or otherwise.
- `error(provider-error)` must have empty `effects` fields. The host rejects
  errors with host mutations.
- The `op-result` arm returned by `continuation.resume(id, results)`
  must match the operation originally suspended under `id`.
- Only operations with a `correlation-id` may suspend. `initialize` remains
  terminal-only until the WIT deliberately gives it a correlation id and a
  `provider-step` return.
- All `provider-return` rules apply to `initialize`'s direct return: successful
  initialization uses only `initialize(initialize-result)`, failed
  initialization uses only `error(provider-error)`, and errors carry no
  effects.
- `shutdown`, `get-config-schema`, and `capabilities` are synchronous
  lifecycle calls. They cannot suspend or carry effects.
- `close-file` and `cancel` are fire-and-forget host calls. They do not return
  `provider-step`, cannot suspend, and cannot carry effects.
- `continuation.cancel(id)` retires the suspended operation. The host must not
  resume that id afterward, and the provider must drop the pending operation
  state for that id.
- `lookup-child-result::not-found` is a normal negative lookup. A missing
  `list-children` target returns `error(provider-error::not-found)`.
- A `lookup-child-result::subtree(tree)` or `list-children-result::subtree(tree)` is a terminal for subtree handoff; the host resolves the handle to a bind-mounted clone directory. Bare subtree results without a resolvable host-side tree ref are provider contract errors.
- `file-attrs` carries metadata only. Projected byte availability lives in
  `file-proj.bytes`.
- `file-proj.bytes` is the only byte source on an `fs` effect entry. Direct
  `lookup-child` and `list-children` entries may also include
  `proj-bytes::inline(...)`, but those bytes belong only to the returned entry,
  not to adjacent or nested paths.
- `read-file-bytes::blob(blob-id)` must name a host-cached blob previously
  returned by `fetch-blob` to this provider runtime. In this WIT version,
  `fetch-blob` is the only `blob-id` producer; `open-archive` returns a
  `tree-ref` and `read-blob` returns bytes. Unknown blob ids are provider
  contract errors.
- `read-file` materializes `proj-bytes::deferred(read-mode::full)` files only.
  Ranged deferred files use `open-file` plus `read-chunk`; inline projected
  files should be served from the projection cache without calling the
  provider.
- `open-file` requires the path to have been projected earlier as
  `proj-bytes::deferred(read-mode::ranged)`. The open result refreshes
  metadata and returns a handle; it does not declare read mode.

## SDK surface

### `Cx` is callout-only

`Cx` is the callout context: `.http()`, `.git()`, `.archives()`, and
`.blob()` yield `CalloutFuture`s. Terminal host mutations are staged on
return builders, not on `Cx`.

### Effects and the return builder

Provider authors do not construct `ProviderReturn` directly in normal code. Effects accumulate on the projection or file-content builders and are emitted as part of the terminal return. The three effect channels in the `effects` record are:

- `canonical`: `canonical-store { id, validator, bytes, view-leaves }` object-cache writes. Emitted automatically by `Load::Fresh` for object-shaped providers; structural providers never emit these.
- `fs`: `fs-write { id, path, kind }` view-cache writes for adjacent or nested paths learned while serving the request. Use projection preload methods such as `preload_file` and `preload_dir` to attach these to the terminal.
- `invalidations`: `object(logical-id)` or `listing(path-or-prefix)` entries applied after the return is accepted. Use the projection invalidation helpers to attach these to the terminal.

### `on-event` returns effects

The `on-event` handler returns a normal provider step. Invalidation effects are
committed at the same accepted-return boundary as lookup, listing, and read
effects:

```rust
async fn on_event(cx: Cx<State>, event: ProviderEvent) -> Result<Effects>
```

Invalidation example:

```rust
Ok(Invalidations::new().prefix("octocat/Hello-World"))
```

The macro wraps this into a provider return with `result: OpResult::OnEvent`.

### Subtree dispatch from inside namespace

`lookup_child` and `list_children` check for treeref handlers first by exact path. On hit, they return `LookupChildResult::Subtree(tree_ref)` or `ListChildrenResult::Subtree(tree_ref)`. Providers register treeref handlers with `r.treeref(t).handler(h)`; the host resolves the tree ref to a bind-mounted clone directory. Provider authors do not write subtree routing by hand; the router macro stages it.

### `CalloutFuture`

```rust
pub struct CalloutFuture<'cx, S, T> { ... }
```

The internal yield/resume mechanics suspend the operation as
`ProviderStep::suspended(callouts)` and resume on the matching
`callout-result` arms. `http.rs`, `git.rs`, `blob.rs`, and `archives.rs`
all return `CalloutFuture<_, _, T>` for their typed responses.

### Provider call sites

- View-cache writes for adjacent paths: `FileProjection::preload_file(path, proj)`, `DirProjection::preload_file(path, proj)`, and `DirProjection::preload_dir(path, proj)` become `fs` effect entries.
- Object-cache writes: emitted automatically by `Load::Fresh` and by directory-carried canonical effects; the `canonical` effect carries `id`, `validator`, `bytes`, and `view-leaves`.
- Invalidation: `FileProjection::invalidate_path(...)`, `invalidate_prefix(...)` — these become `invalidations` effect entries.
- Event handlers: `on-event` returns a normal provider step; invalidations ride in terminal `effects.invalidations`.

## Host runtime

### Runtime types

The runtime types that drive provider steps and dispatch callouts use
the protocol's vocabulary:

- `Runtime`.
- `Callout::execute`.
- `Callouts::execute`.
- `drive_provider`.

The runtime owns a mounted provider instance. `Op::execute` models the
host-to-provider operation call, `Callouts::execute` models a suspended batch,
and `Callout::execute` owns per-callout dispatch. Effects are not executed
through the callout path; they are validated and applied once when a provider
return is accepted.

### Driving provider steps

```rust
async fn drive_provider(
    &self,
    id: u64,
    mut step: ProviderStep,
    op: Op,
) -> Result<OpResult> {
    loop {
        match step {
            ProviderStep::Returned(ret) => {
                return self.finish_provider_return(&op, ret);
            }
            ProviderStep::Suspended(callouts) => {
                let results = Callouts::new(id, &callouts)?.execute(self).await;
                step = self.resume_provider(id, results)?;
            }
        }
    }
}
```

There are no trailing callouts after return. Once a provider returns, the host
validates the result and effects, applies the effects, and hands the result to
FUSE.

### Effect application

The host `Materializer::apply` owns host-side mutation from a `provider-return`:

- `effects.canonical` entries are written to the object cache: `object_store(anchor, canonical, leaves, op_gen)`. Each entry also evicts the anchor's prior derived view leaves before storing new ones (preventing mixed-version views).
- `effects.fs` entries are written to the view cache: `project(fs_write, op_gen)`. The dirent read-modify-write is transactional inside the cache.
- `effects.invalidations` entries delete exact or prefix-matched cache records and notify the kernel.

Effect validation happens before application. Invalid effects fail the
operation as a provider contract error and do not partially mutate host state.

### Direct result projection

Accepted `lookup-child` and `list-children` results install their direct
entries as part of the primary operation result. If a returned entry is
`file(file-proj { bytes: proj-bytes::inline(...) })`, the host commits those
bytes for that entry alongside the lookup or listing view cache records.

`effects.fs` is the channel for adjacent, nested, or otherwise
secondary paths that were learned while serving the operation.

### Subtree handoff in the namespace pipeline

`call_lookup_child` and `call_list_children` inspect the returned
result. If it carries a subtree variant, the host resolves the tree ref to a
bind-mounted clone directory and returns to FUSE.
`opendir_via_provider` treats `ListChildrenResult::Subtree(tree_ref)` as
an ordinary list result.

### View cache writes

View cache writes come from `effects.fs`. Directory listings
populate immediate dirent records from the returned `dir-listing`; extra
adjacent or nested paths come from `effects.fs` entries.

Read results that return `read-file-bytes::blob(blob-id)` must reference
a host-cached blob id the runtime can resolve. `fetch-blob` is the only
`blob-id` producer in this WIT version; if a future callout also
produces blob ids, this validation rule must name that callout
explicitly. Unknown blob ids fail as provider contract errors before
FUSE receives data.

### Git callouts

`git-open-repo` is the sole git callout. Repo browsing happens through
FUSE passthrough over the tree returned by `git-open-repo`; there are
no `git-list-tree`, `git-read-blob`, `git-head-ref`, or
`git-list-cached-repos` arms.

## Test surfaces

These test files encode the protocol contract and should be the first
stop when changing protocol semantics:

- `crates/omnifs-itest/tests/<provider>/` — provider-specific
  per-operation callout expectations (HTTP fetch, blob fetch, etc.) plus
  returned-effect assertions for preload-style and invalidation paths.
- `crates/omnifs-host/tests/runtime_test.rs` — `ProviderStep::Returned`
  matching, validation of `ProviderReturn { result, effects }` shape.
- `crates/omnifs-host/tests/callout_tracing.rs` — span / event field contract
  for the `omnifs_callout` tracing target.
- `crates/omnifs-sdk/tests/error_api_test.rs` — `error(provider-error)`
  through `ProviderStep::Returned`.
- `crates/omnifs-sdk/src/router/tests.rs` — `Router` dispatch exercised
  through `lookup-child`, `list-children`, and `read-file`.

## Out of scope

1. **Per-operation WIT interface split** (`namespace-lookup`, `namespace-list`,
   etc.). The Rust macro layer still enforces operation-specific return types;
   the remaining WIT laxity is acceptable for now.
2. **Mid-operation partial results**. FUSE is request/response per operation.
   Partial results do not reach users. Cross-call readdir pagination remains
   separate work.
3. **Cross-call readdir pagination**. This depends on end-to-end
   `Listing::partial` + `next_cursor` / `DirProjection` cursor wiring and is
   independent of this protocol shape.
4. **Per-correlation timeout for guest and git**. This is a separate runtime
   policy issue.
5. **Non-cache effects beyond the three channels**. If future providers need additional terminal host mutations, add explicit fields to the `effects` record. Do not tunnel them through callouts.

## Behavioral notes

- **`lookup-child-result::not-found` drops siblings.** A not-found result is bare.
  If a future use case needs "target missing but here is parent context," add a
  dedicated result arm instead of smuggling cache updates through the miss.
- **`on-event` is intentionally no-data.** Event completion is the result;
  invalidation or no-op behavior belongs in the returned `Invalidations`.
- **Effects on `error`.** `provider-return.result = error(...)` must carry empty effects fields. Errors fail the operation and cannot commit host mutations.
- **`effects.fs` versus list.** A listing result says what immediate children the
  operation returns. An `effects.fs` entry says what additional provider-relative
  path metadata or bytes the host should install. Inline bytes on a direct
  listed or looked-up file belong to that returned entry and are installed with
  the primary result, not through a secondary effect.
- **Siblings versus listing.** `lookup-entry.siblings + exhaustive` stays as a
  lookup-specific contract. If `exhaustive` is true, absence from
  `target + siblings` is a sound negative lookup for the parent. This is not a
  general `dir-listing` result and should not be broadened accidentally.
- **Subtree result.** A subtree result says the requested path is a treeref handoff. The host resolves the tree ref to a bind-mounted clone directory.
- **Read bytes consistency.** `file-proj.bytes` describes the projected file
  contract. `read-file-result` carries materialized full-read bytes and must
  be consistent with `proj-bytes::deferred(read-mode::full)`.
