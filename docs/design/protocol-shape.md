# Protocol shape: callouts, returns, and effects

Status: draft, ready to implement
Scope: `wit/provider.wit`, host runtime, SDK, SDK macros, providers, tests
Branch: open TBD

## Goal

Tighten the provider protocol vocabulary so each semantic channel has one
name and one place in the WIT:

- `callout`: intermediate work the provider asks the host to run, then
  receives through `continuation.resume`.
- `return`: the completed operation answer.
- `effect`: a host-side mutation committed because a return was accepted.

The important sentence is: a provider step either suspends on callouts or
returns a result with effects.

This also folds sidecar dispatches like `materialize` into their parent
operation, removes dead git callouts, and keeps variant names aligned with
provider contract rather than host implementation.

## Current state

After the earlier effect-boundary refactor, the protocol still uses one
outer response shape for both intermediate and completed states:

```wit
variant effect {
    fetch(http-request),
    stream-open / stream-recv / stream-close,
    ws-connect / ws-send / ws-recv / ws-close,
    git-open-repo / git-list-tree / git-read-blob / git-head-ref / git-list-cached-repos,
    preload-paths(list<preloaded-file>),
    invalidate-path(string),
    invalidate-prefix(string),
}

variant effect-result {
    http-response, stream-opened, stream-chunk, ws-*, git-*,
    ack,
    effect-error(effect-error),
}

variant action-result {
    dir-entries(dir-listing),
    lookup(lookup-result),
    file(file-content-result),
    not-materialized,
    disowned-tree(tree-ref),
    ok,
    provider-err(provider-error),
    provider-initialized(provider-info),
}

record provider-response {
    effects: list<effect>,
    terminal: option<action-result>,
}

interface browse {
    lookup-child, list-children, read-file, materialize, ...
}
```

Four problems:

1. `effect` conflates intermediate host work with terminal host mutation.
   HTTP fetches and git opens are not effects in the protocol sense; they are
   callouts. Cache projection and invalidation are effects.
2. `provider-response` conflates progress with completion. A callout-only
   response is not a provider return; it is a suspended provider step.
3. `action-result` contains variants that belong to specific operations:
   `not-materialized` and `disowned-tree` belong to `materialize`,
   `provider-initialized` belongs to `initialize`, and `ok` is the degenerate
   event result.
4. `materialize` is a separate browse method whose sole job is to answer "is
   this path a subtree handoff?" The host calls it while serving
   `lookup-child`, `list-children`, or `opendir`, so the sidecar method adds a
   dispatch step and two result arms without adding a distinct operation.

## Target WIT

Types not redefined in this block are kept from the current
`wit/provider.wit` unless a later phase explicitly changes them.

```wit
type correlation-id = u64;
type stream-id = u64;
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
    stream-open(http-request),
    stream-recv(stream-id),
    stream-close(stream-id),
    ws-connect(ws-connect-request),
    ws-send(ws-send-request),
    ws-recv(ws-recv-request),
    ws-close(stream-id),
    git-open-repo(git-open-request),
    fetch-blob(blob-fetch-request),
    open-archive(archive-open-request),
    read-blob(read-blob-request),
}

variant callout-result {
    http-response(http-response),
    stream-opened(stream-id),
    stream-chunk(option<list<u8>>),
    stream-closed,
    ws-connected(stream-id),
    ws-message(option<list<u8>>),
    ws-closed,
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
    /// lookup or listing result. Use `effect::project` for additional paths.
    kind: entry-kind,
}

/// Completed operation answer. This shape cannot carry callouts.
/// `effects` is the only terminal host-mutation channel in the target WIT.
record provider-return {
    result: operation-result,
    effects: list<effect>,
}

/// Host-side mutation committed at the return boundary.
variant effect {
    project(proj-entry),
    invalidate-path(string),
    invalidate-prefix(string),
    disown-tree(tree-handoff),
}

/// Provider-relative path metadata to install into the host projection cache.
record proj-entry {
    path: string,
    kind: entry-kind,
}

/// Host routing state for a tree handoff. The operation result still says
/// that the path is a subtree; this effect says the host should install that
/// tree as disowned or passthrough state after accepting the return.
record tree-handoff {
    /// Provider-relative path being handed off. For `lookup-child`, this is
    /// `parent-path/name`; for `list-children`, this is the listed path.
    path: string,
    tree: tree-ref,
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
variant operation-result {
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

interface browse {
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
- `error(provider-error)` must have an empty `effects` list. The host rejects
  errors with host mutations.
- The `operation-result` arm returned by `continuation.resume(id, results)`
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
- A `lookup-child-result::subtree(tree)` or
  `list-children-result::subtree(tree)` must be paired with exactly one
  matching `effect::disown-tree(tree-handoff { path, tree })`. For
  `lookup-child`, `path` is the provider-relative looked-up child path
  (`parent-path/name`). For `list-children`, `path` is the listed path. Bare
  subtree results are provider contract errors.
- `file-attrs` carries metadata only. Projected byte availability lives in
  `file-proj.bytes`.
- `file-proj.bytes` is the only byte source on a `project` effect. Direct
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

## Shape shifts

- `provider-step` owns progress. It is either `suspended(callouts)` or
  `returned(provider-return)`.
- `provider-return` is now a real return. It cannot carry callouts.
- `callout` is strictly request/response. Fire-and-forget is gone.
- `effect` is strictly terminal host mutation. It is only present inside
  `provider-return`, so it is structurally tied to a completed operation.
- `file-attrs` stops carrying byte availability. `file-proj` combines
  metadata with `proj-bytes` for projected files, and `read-file-result`
  carries materialized read bytes separately.
- `preload`, `sibling-files`, and terminal-specific invalidation fields collapse
  into `effect::project` and `effect::invalidate-*`.
- `subtree(tree-ref)` remains result data for `lookup-child` or
  `list-children`. Every subtree result must include the matching
  `effect::disown-tree` so the host can commit routing state at the return
  boundary.
- `action-result` becomes `operation-result`; every non-`error` arm corresponds
  to an operation.
- Every `operation-result` arm carries the matching `<operation>-result` type.
  `error(provider-error)` is the only cross-operation arm.
- `materialize` deletes. `#[subtree]` handlers dispatch from inside
  `lookup-child` and `list-children`.
- `ack` deletes. No callout needs positional padding for a fire-and-forget
  mutation.
- Dead git callouts (`git-list-tree`, `git-read-blob`, `git-head-ref`,
  `git-list-cached-repos`) delete. Repo browsing uses FUSE passthrough over the
  tree returned by `git-open-repo`.

## Rename table

| Old | New | Reason |
|---|---|---|
| `provider-response` | `provider-step` | The outer shape is progress, not necessarily a return. |
| callout-shaped `effect` arms | `callout` | HTTP, stream, websocket, git, blob, and archive operations suspend the provider and resume with results. |
| host-mutation `effect` arms | `effect` | Projection, invalidation, and disowning are terminal host-side mutations. |
| `effect-result` | `callout-result` | Only callouts have response values. |
| `effect-error` | `callout-error` | Error from host-run callout work. |
| `effect-results` | `callout-results` | Continuation resume values for suspended callouts. |
| `resume` interface | `continuation` interface | The interface owns both resume and cancellation of suspended operations. |
| `action-result` | `operation-result` | The result arm corresponds to the provider operation. |
| `file-attrs.bytes` | `file-proj.bytes` | Bytes are projection data, not file metadata. |
| `projected-entry` / `projected-file` | `proj-entry` / `file-proj` | `proj` is the projection data vocabulary; one `project` effect installs one path. |
| `dir-entries(dir-listing)` | `list-children(list-children-result::entries(...))` | Arm and result type match the operation. |
| `file(file-content-result)` | `read-file(read-file-result)` | Arm and result type match the operation. |
| `provider-initialized(provider-info)` | `initialize(initialize-result)` | Arm and result type match lifecycle operation. |
| `provider-err(provider-error)` | `error(provider-error)` | Cross-operation failure arm. |
| `cache-preload` callout arm | `effect::project(proj-entry)` | Projection is committed at the return boundary. |
| `cache-invalidate-path` callout arm | `effect::invalidate-path` | Invalidation is committed at the return boundary. |
| `cache-invalidate-prefix` callout arm | `effect::invalidate-prefix` | Invalidation is committed at the return boundary. |
| `ack` callout-result arm | removed | No fire-and-forget callouts remain. |
| `not-materialized` action-result arm | `lookup-child-result::not-found` | Fold into owner operation. |
| `disowned-tree(tree-ref)` action-result arm | `lookup-child-result::subtree`, `list-children-result::subtree`, plus matching `effect::disown-tree(tree-handoff)` | Result says what the path is; effect says what host state changes. |
| `ok` action-result arm | `on-event` | Event handling can complete with no effects. |
| `browse.materialize` method | removed | Folds into `lookup-child` and `list-children`. |
| `mutations-planned(...)` action-result arm | `plan-mutations(plan-mutations-result)` | Arm and result type match the operation. |
| `mutation-executed(mutation-outcome)` action-result arm | `execute(execute-result)` | Arm and result type match the operation. |
| `resource-files(list<file-entry>)` action-result arm | `fetch-resource(fetch-resource-result)` | Arm and result type match the operation. |
| `EffectRuntime` | `CalloutRuntime` | Runtime still drives callouts. Effects are applied at the return boundary. |
| `EffectFuture` | `CalloutFuture` | Futures suspend on callouts. |
| `execute_single_effect` | `execute_single_callout` | Only callouts are executed and resumed. |
| `drive_effects` | `drive_provider_step` | The host drives suspended steps until a return appears, then applies effects. |

## SDK impact

### `Cx` loses host-mutation helpers

```rust
// Removed from Cx.
impl<S> Cx<S> {
    pub fn preload_paths<I, P, B>(&self, files: I) { ... }
    pub fn invalidate_path(&self, path: impl Into<String>) { ... }
    pub fn invalidate_prefix(&self, prefix: impl Into<String>) { ... }
}
```

`Cx` remains the callout context: `.http()`, `.git()`, `.archives()`, and
`.blob()` yield `CalloutFuture`s. Terminal host mutations move to return
builders.

### `Effects` builder

```rust
#[derive(Default)]
pub struct Effects {
    effects: Vec<Effect>,
}

impl Effects {
    pub fn new() -> Self;
    pub fn project_dir(&mut self, path: impl Into<String>) -> Result<&mut Self>;
    pub fn project_file(&mut self, path: impl Into<String>, file: FileProj) -> Result<&mut Self>;
    pub fn invalidate_path(&mut self, path: impl Into<String>) -> &mut Self;
    pub fn invalidate_prefix(&mut self, prefix: impl Into<String>) -> &mut Self;

    #[doc(hidden)]
    pub fn disown_tree(&mut self, path: impl Into<String>, tree: TreeRef) -> &mut Self;
}
```

Provider authors do not construct `ProviderReturn` directly in normal code.
The macro wraps handler results with their accumulated effects. `disown_tree`
is reserved for generated subtree plumbing; ordinary provider code returns a
subtree result and lets the macro stage the required handoff effect.

### `Projection` stages projection effects

`Projection` continues to describe the direct list/lookup answer. It also
gains helpers for adjacent or nested projected paths that should be installed
when the answer is accepted:

```rust
impl Projection {
    pub fn proj(&mut self, path: impl Into<String>, content: impl Into<Vec<u8>>);
    pub fn proj_dir(&mut self, path: impl Into<String>);
    pub fn proj_file(&mut self, path: impl Into<String>, file: FileProj);
}
```

Old preload helpers become `projection.proj(...)` for inline bytes or
`projection.proj_file(...)` for an explicit `FileProj`. The name now matches
the data type: the provider is asking the host to install a projection after
the completed operation answer is accepted.

### `FileContent` can carry effects

`FileContent` loses `sibling-files`. A read handler that learned adjacent files
uses the same effect channel:

```rust
let mut effects = Effects::new();
effects.project_file(path, file_proj)?;
FileContent::new(bytes).with_effects(effects)
```

The host no longer needs separate sibling-file plumbing for read terminals.

### `on-event` returns effects

The user-facing signature becomes:

```rust
async fn on_event(cx: Cx<State>, event: ProviderEvent) -> Result<Effects>
```

The default implementation returns `Ok(Effects::new())`. Invalidation becomes:

```rust
let mut effects = Effects::new();
effects.invalidate_prefix("octocat/Hello-World");
Ok(effects)
```

The macro wraps this as:

```rust
ProviderStep::returned(ProviderReturn {
    result: OperationResult::OnEvent,
    effects: effects.into_wit(),
})
```

### Subtree dispatch moves into `MountRegistry`

Today the macro generates a `materialize` function that calls
`MountRegistry::materialize`, which scans `#[subtree]` handlers. Post-fold,
`lookup_child` and `list_children` check subtree handlers first by exact path.
On hit, they return `LookupChildResult::Subtree(tree_ref)` or
`ListChildrenResult::Subtree(tree_ref)` and stage the required matching
`effect::disown-tree` so the host installs passthrough routing state at the
same return boundary. Provider authors do not write this effect by hand.

The generated `materialize` export and `MountRegistry::materialize` method
delete.

### `EffectFuture` renames to `CalloutFuture`

```rust
pub struct CalloutFuture<'cx, S, T> { ... }
```

All uses in `http.rs`, `git.rs`, `blob.rs`, and `archives.rs` follow. The
internal yield/resume mechanics are unchanged, but the outer type now returns
`ProviderStep::suspended(callouts)` instead of a callout-bearing return.

### Provider call sites

- Old preload helpers -> `projection.proj(...)`, `projection.proj_file(...)`,
  or `effects.project_file(...)`.
- `cx.invalidate_path(...)` / `cx.invalidate_prefix(...)` ->
  `effects.invalidate_path(...)` / `effects.invalidate_prefix(...)`.
- `ProviderResponse::terminal(ActionResult::Ok)` in `on-event` ->
  `Ok(Effects::new())`.

Providers affected: `github` handlers and events, `test`, and any provider
tests that directly match generated WIT enums. DNS keeps its user-facing logic
but updates old `Effect` imports and test strings to `Callout`.

## Host impact

### Runtime naming

- `EffectRuntime` -> `CalloutRuntime`.
- `execute_single_effect` -> `execute_single_callout`.
- `execute_batch` stays named.
- `drive_effects` -> `drive_provider_step`.

The runtime still executes callouts. Effects are not executed through the
callout path; they are applied once when a provider return is accepted.

### Driving provider steps

```rust
async fn drive_provider_step(&self, id: u64, mut step: ProviderStep) -> Result<OperationResult> {
    loop {
        match step {
            ProviderStep::Returned(ret) => {
                validate_return(&ret)?;
                self.apply_effects(&ret.effects)?;
                return Ok(ret.result);
            }
            ProviderStep::Suspended(callouts) if callouts.is_empty() => {
                return Err(RuntimeError::ProviderError("empty suspended step".into()));
            }
            ProviderStep::Suspended(callouts) => {
                let results = self.execute_batch(&callouts).await;
                let mut store = self.store.lock();
                step = self.bindings.omnifs_provider_continuation().call_resume(
                    &mut *store, id, &results,
                )?;
            }
        }
    }
}
```

There are no trailing callouts after return. Once a provider returns, the host
validates the result and effects, applies the effects, and hands the result to
FUSE.

### Effect application

`apply_effects` owns host-side mutation:

- `project` writes lookup/attr records for the provider-relative path and
  inline bytes when the projected kind is
  `file-proj { bytes: proj-bytes::inline(...) }`.
- `invalidate-path` deletes exact cache records and notifies the kernel.
- `invalidate-prefix` deletes prefix cache records and notifies affected
  kernel paths.
- `disown-tree` installs or updates passthrough routing state for a returned
  subtree.

Effect validation happens before application. Invalid effects fail the
operation as a provider contract error and do not partially mutate host state.

### Direct result projection

Accepted `lookup-child` and `list-children` results install their direct
entries as part of the primary operation result. If a returned entry is
`file(file-proj { bytes: proj-bytes::inline(...) })`, the host commits those
bytes for that entry alongside the lookup or listing cache records.

`effect::project` remains the channel for adjacent, nested, or otherwise
secondary paths that were learned while serving the operation.

### Subtree handoff folds into browse pipeline

`try_subtree_handoff` and `call_materialize` delete. In their place,
`call_lookup_child` and `call_list_children` inspect the returned result. If it
carries a subtree variant, the host validates and applies the matching
`disown-tree` effect before returning to FUSE.

`opendir_via_provider` stops special-casing materialization. It calls
`call_list_children` and handles `ListChildrenResult::Subtree(tree_ref)` as an
ordinary list result.

The validator checks the handoff path: `lookup-child` handoffs must target the
looked-up child path, and `list-children` handoffs must target the listed path.

### Projection cache writes

`browse_pipeline::strip_projected_files` disappears with `sibling-files`.
Projection cache writes now come from `effect::project`. Directory listings
still populate immediate dirent records from the returned `dir-listing`; extra
adjacent or nested paths come from effects.

Read results that return `read-file-bytes::blob(blob-id)` must reference a
host-cached blob id the runtime can resolve. The current source set is only
`fetch-blob`; if a future callout also produces blob ids, this validation rule
must name that callout explicitly. Unknown blob ids fail as provider contract
errors before FUSE receives data.

### Dead git cleanup

- `wit/provider.wit`: delete `git-list-tree`, `git-read-blob`, `git-head-ref`,
  and `git-list-cached-repos`; keep `git-open-repo`, `git-open-request`, and
  `git-repo-info`.
- `crates/host/src/runtime/mod.rs::execute_single_callout`: delete the removed
  git match arms.
- `crates/host/src/runtime/git.rs::GitExecutor`: delete `list_tree`,
  `read_blob`, `head_ref`, `list_cached_repos`, and their helpers. Keep
  `open_repo` and tree-ref resolution.
- `crates/host/tests/git_executor_test.rs`: delete tests for removed methods;
  keep open-repo coverage if it remains useful.
- `crates/omnifs-sdk/src/git.rs`: delete `Builder::list_tree`,
  `Builder::read_blob`, and `Builder::head_ref`.

## Tests

- `crates/host/tests/provider_routes_test.rs`: update `Effect` to `Callout`
  for intermediate work and add assertions for returned effects where tests
  previously inspected preloads or invalidations.
- `crates/host/tests/runtime_test.rs`: match provider progress as
  `ProviderStep::Returned(ProviderReturn { result: ..., effects: ... })`.
- `crates/omnifs-sdk/tests/error_api_test.rs`: match returned errors through
  `ProviderStep::Returned`.
- `crates/omnifs-sdk-macros/tests/path_first_provider.rs`: subtree tests
  exercise `lookup-child` and `list-children`; no test calls `materialize`.
- `on-event` tests assert invalidation effects, not result fields.
- Projection tests assert `project` effects, not `preload` or `sibling-files`.

## Migration

This is a breaking WIT change and a breaking Rust SDK change. Pre-v1, no
back-compat path is needed. Make a clean cut in one PR and rebuild all provider
Wasm artifacts after regenerating bindings.

## Out of scope

1. **Per-operation WIT interface split** (`browse-lookup`, `browse-list`,
   etc.). The Rust macro layer still enforces operation-specific return types;
   the remaining WIT laxity is acceptable for now.
2. **Mid-operation partial results**. FUSE is request/response per operation.
   Partial results do not reach users. Cross-call readdir pagination remains
   separate work.
3. **Cross-call readdir pagination**. This depends on end-to-end
   `PageStatus::More(cursor)` wiring and is independent of this protocol shape.
4. **Per-correlation timeout for guest and git**. This is a separate runtime
   policy issue.
5. **Non-cache effects beyond disowning**. If future providers need additional
   terminal host mutations, add explicit `effect` arms. Do not tunnel them
   through callouts.

## Behavioral notes

- **`lookup-child-result::not-found` drops siblings.** A not-found result is bare.
  If a future use case needs "target missing but here is parent context," add a
  dedicated result arm instead of smuggling cache updates through the miss.
- **`on-event` is intentionally no-data.** Event completion is the result;
  invalidation, projection, or no-op behavior belongs in `effects`.
- **Effects on `error`.** `provider-return.result = error(...)` must carry no
  effects. Errors fail the operation and cannot commit host mutations.
- **Project versus list.** A listing result says what immediate children the
  operation returns. A project effect says what additional provider-relative
  path metadata or bytes the host should install. Inline bytes on a direct
  listed or looked-up file belong to that returned entry and are installed with
  the primary result, not through a secondary effect.
- **Siblings versus listing.** `lookup-entry.siblings + exhaustive` stays as a
  lookup-specific contract. If `exhaustive` is true, absence from
  `target + siblings` is a sound negative lookup for the parent. This is not a
  general `dir-listing` result and should not be broadened accidentally.
- **Subtree versus disown.** A subtree result says what the requested path is.
  The matching disown effect says what routing state the host commits after
  accepting that answer. A bare subtree result is a provider contract error,
  and a handoff whose path does not match the returned subtree path is also an
  error.
- **Read bytes consistency.** `file-proj.bytes` describes the projected file
  contract. `read-file-result` carries materialized full-read bytes and must
  be consistent with `proj-bytes::deferred(read-mode::full)`.

## Implementation plan

This should land as one coordinated breaking protocol change. The internal
pause points below are useful for review, but do not leave the branch with only
the WIT or only one side of the host/SDK boundary migrated.

This is also a cleanup task. Do not preserve compatibility aliases, transitional
dual terminology, wrapper shims, or old protocol paths to make the migration
look smaller. When dead code, awkward names, duplicated concepts, TODOs,
compatibility residue, or implementation slop appear, remove or normalize them
in the same pass unless doing so would change unrelated product behavior.

The quality bar is that the final code reads like it was written by a
thoughtful principal-level Rust engineer: narrow domain types, explicit
protocol invariants, idiomatic pattern matching, small cohesive functions,
precise errors, and names that match the WIT. Prefer direct Rust over clever
generic scaffolding. Do not add broad abstractions unless they remove real
complexity at this boundary.

### Phase 1: WIT and binding surface

Edit `wit/provider.wit` first:

- Add `provider-step`, make `provider-return` terminal-only, and move
  intermediate work into `callout`.
- Split file metadata from bytes: `file-attrs` keeps metadata, `file-proj`
  carries `attrs + proj-bytes`, and `read-file-result` carries
  `attrs + read-file-bytes`.
- Collapse projection mutation into `effect::project(proj-entry)`, rename the
  disown data to `tree-handoff`, and keep invalidation effects as terminal
  host mutations.
- Replace `action-result` with the symmetric `operation-result` shape.
- Rename the `resume` interface to `continuation` and keep `resume` as the
  method that delivers `callout-results`.
- Keep `initialize` terminal-only. If initialize-time callouts become necessary
  later, add a correlation id and promote it to `provider-step` explicitly.
- Remove `materialize`, `ack`, fire-and-forget callouts, terminal preload
  fields, sibling-file fields, and the dead git callouts.

Then build just enough to expose generated-binding fallout:

```bash
cargo check -p omnifs-sdk
cargo check -p omnifs-host
```

This gate is diagnostic while later phases are still unimplemented. The useful
signal is that the WIT parses and binding generation reaches the expected Rust
fallout, not that downstream crates compile yet.

### Phase 2: SDK core

Update `crates/omnifs-sdk/src/file_attrs.rs` so `FileAttrs` contains only
metadata and introduce `FileProj`, `ProjBytes`, and `ReadFileBytes`.

Update `crates/omnifs-sdk/src/browse.rs`, `handler.rs`, `cx.rs`, and the
callout helpers:

- Rename `EffectFuture` to `CalloutFuture`.
- Remove `Cx::preload_paths`, `Cx::invalidate_path`, and
  `Cx::invalidate_prefix`.
- Add an `Effects` builder around terminal host mutations.
- Update `Projection` to stage `proj` entries while preserving
  `siblings + exhaustive` lookup semantics.
- Convert `FileContent` into the new `read-file-result { attrs, bytes }`
  shape and move adjacent projected paths into effects.

Validation gate:

```bash
cargo check -p omnifs-sdk -p omnifs-sdk-macros
```

### Phase 3: SDK macros

Update `crates/omnifs-sdk-macros/src/provider_macro.rs` and related macro tests:

- Generate `provider-step::suspended` for callout suspension and
  `provider-step::returned` for completed operation answers.
- Track each resumable correlation id's originating operation and reject mismatched
  `operation-result` arms after `continuation.resume`.
- Stop generating `materialize`.
- Generate `continuation` instead of `resume` exports.
- Wrap `on-event` as `operation-result::on-event` plus effects.

Validation gate:

```bash
cargo test -p omnifs-sdk-macros
```

### Phase 4: Host runtime

Update `crates/host/src/runtime/mod.rs` and runtime helpers:

- Rename `EffectRuntime` to `CalloutRuntime` and
  `execute_single_effect` to `execute_single_callout`.
- Replace response driving with `drive_provider_step`.
- Add `validate_return` for empty suspended steps, operation/result agreement,
  empty effects on `error`, direct result byte validity, blob id resolution,
  and subtree/disown pairing.
- Add `apply_effects` for `project`, invalidation, and `disown-tree`.
- Delete dead git callout handling and dead git executor methods.

Validation gate:

```bash
cargo test -p omnifs-host --test runtime_test
cargo test -p omnifs-host --test provider_routes_test
```

### Phase 5: Host browse and FUSE integration

Update `crates/host/src/runtime/browse_pipeline.rs`,
`crates/host/src/fuse/mod.rs`, and cache write sites:

- Replace preload and sibling-file stripping with `effect::project`
  application.
- Preserve `lookup-entry.siblings + exhaustive` behavior exactly. If
  `exhaustive` is true, absence from `target + siblings` remains a sound
  negative lookup for that parent. If false, siblings remain cache hints only.
- Fold subtree handoff into `lookup-child` and `list-children`; require the
  matching `tree-handoff` effect before installing passthrough routing.
- Route inline projected files from the projection cache, full deferred files
  through `read-file`, and ranged deferred files through `open-file` /
  `read-chunk`.

Validation gate:

```bash
cargo test -p omnifs-host --test cache_l0_test
cargo test -p omnifs-host --test cache_l2_test
cargo test -p omnifs-host --test provider_routes_test
```

### Phase 6: Providers and tools

Update `providers/github`, `providers/dns`, `providers/arxiv`,
`providers/test`, and `crates/omnifs-tool-archive`:

- Replace preloads with `proj` effects or projection helpers.
- Replace event invalidation helpers with returned effects.
- Update file declarations to `file-proj { attrs, bytes }`.
- Update read handlers to return `read-file-result { attrs, bytes }`.
- Remove any use of deleted git callouts.

Validation gate:

```bash
just check-providers
just build-providers
```

### Phase 7: Repo-wide cleanup and final validation

Update docs, tests, and repo guidance so `callout`, `return`, `effect`, `proj`,
and `operation-result` are used consistently.

Cleanup is not a separate follow-up phase. During the migration, keep removing
the old shape as soon as each replacement is in place. At the end, do a
repo-wide pass for:

- Old protocol names, compatibility aliases, and deleted callout/result arms.
- Dead helper methods and tests for `materialize`, preloads, sibling-files, old
  git callouts, and old effect/callout naming.
- Temporary comments, TODOs, compatibility notes, and "new protocol" versus
  "old protocol" split-brain language.
- Overly broad helper layers introduced only to make the refactor compile.
- Error messages that describe implementation plumbing instead of the provider
  contract violation.

Final validation:

```bash
wasm-tools component wit wit/provider.wit
cargo fmt --all --check
just check
just test-integration
just dev
docker exec omnifs /bin/zsh -lc 'omnifs status'
docker exec omnifs /bin/zsh -lc 'OMNIFS_DEMO_MODE=smoke /tmp/demo.sh'
docker exec omnifs /bin/zsh -lc 'tail -200 /tmp/omnifs.log'
```

The Docker smoke gate matters because provider Wasm artifacts can otherwise
mask an SDK or WIT migration mistake.

Cleanup gates:

```bash
rg 'EffectFuture|EffectRuntime|execute_single_effect|drive_effects|action-result|op-result|provider-response|effect-result|effect-error|preload|sibling-files|materialize|not-materialized|disowned-tree|ack|git-list-tree|git-read-blob|git-head-ref|git-list-cached-repos' wit crates providers
rg 'TODO|FIXME|compat|legacy|temporary|shim|old protocol|new protocol' wit crates providers docs/design
cargo clippy -- -D warnings
just check-providers
```

The first search should return only intentional historical references in design
docs or rename tables. Any hit in WIT, runtime, SDK, macros, providers, or
tests is suspect and should be removed or renamed. The second search should not
leave migration debt behind; if a note is still needed, rewrite it as a normal
contract explanation instead of a transitional comment.

Final review gates:

- Read the full diff once for naming symmetry: operation arm, result type, Rust
  type, helper, test, and error text should all use the same vocabulary.
- Read the host runtime path once as a protocol state machine: callout
  execution, resume, return validation, effect application, and FUSE handoff
  should be obvious without knowing the old implementation.
- Read the SDK and macro surface once as provider-author API: ordinary providers
  should not see disown plumbing, raw `ProviderReturn` assembly, or internal
  callout mechanics unless they are intentionally using low-level APIs.
- If a gate fails, fix the root cause. Do not weaken tests, skip checks, or
  leave cleanup as follow-up work.
