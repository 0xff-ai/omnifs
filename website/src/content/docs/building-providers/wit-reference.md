---
title: WIT Reference
description: The omnifs:provider WIT browse surface — lookup_child, list_children, read_file, result variants, resume, and on-event.
---

This page maps the SDK you write to the raw `omnifs:provider` WIT interface the host drives. You normally work through the SDK types; this is the contract underneath them. All excerpts are from `wit/provider.wit`.

## The provider world

```wit
world provider {
    import log;
    export lifecycle;
    export browse;
    export continuation;
    export notify;
}
```

The `#[omnifs_sdk::provider(...)]` macro implements all four exported interfaces for you. `log` is the only host import the provider may call directly.

## Browse surface

```wit
interface browse {
    use types.{correlation-id, file-handle, provider-step};

    lookup-child: func(id: correlation-id, parent-path: string, name: string)
        -> provider-step;
    list-children: func(id: correlation-id, path: string)
        -> provider-step;
    read-file: func(id: correlation-id, path: string)
        -> provider-step;
    open-file: func(id: correlation-id, path: string)
        -> provider-step;
    read-chunk: func(id: correlation-id, handle: file-handle, offset: u64, len: u32)
        -> provider-step;
    close-file: func(handle: file-handle);
}
```

- `lookup-child` resolves one child by name. Your `#[dir]`/`#[file]`/`#[treeref]`/`#[bind]` handlers feed it; the SDK answers subtree handlers first, then exact/static/auto-navigable shape, then the parent `#[dir]` handler for dynamic children.
- `list-children` lists a directory; built from the same handlers, merging static child shape with the parent directory projection.
- `read-file` returns exact bytes; backed by your `#[file]` handlers.
- `open-file` / `read-chunk` / `close-file` are the **reserved** ranged-read path. The current runtime serves exact bytes via `read-file` and explicit subtree handoff.

## Provider step: return or suspend

Every browse and continuation call returns a `provider-step`:

```wit
variant provider-step {
    suspended(list<callout>),
    returned(provider-return),
}

record provider-return {
    %result: op-result,
    effects: list<effect>,
}
```

`suspended` is **not** an answer — the host must run a non-empty callout batch and resume. `returned` is the completed answer (`op-result`) plus the host-side `effect`s to commit. The SDK builds both for you: a `Result<List>`/`Result<FileContent>` becomes a `returned`, and a `cx.fetch(..)` mid-handler becomes a `suspended` followed by a resume.

```wit
variant op-result {
    lookup-child(lookup-child-result),
    list-children(list-children-result),
    read-file(read-file-result),
    open-file(open-file-result),
    read-chunk(read-chunk-result),
    initialize(initialize-result),
    on-event,
    error(provider-error),
}
```

## Lookup and list results, including subtree

```wit
variant lookup-child-result {
    entry(lookup-entry),
    subtree(tree-ref),
    not-found,
}

record lookup-entry {
    target: dir-entry,
    siblings: list<dir-entry>,
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
```

The `subtree(tree-ref)` arm is the clone/archive handoff. It is **result data**, not the host mutation itself — the actual bind-mount install is staged as a `disown-tree` effect. In the SDK you produce this by returning a `TreeRef` from a `#[treeref]` handler; the SDK emits the `subtree` result and the matching effect. `lookup-entry.siblings` and `dir-listing.entries` are filled from the children you add to a `Projection`; `exhaustive` reflects whether you called `PageStatus::Exhaustive`.

## Read result

```wit
variant read-file-bytes {
    inline(list<u8>),
    blob(blob-id),
}

record read-file-result {
    attrs: file-attrs,
    bytes: read-file-bytes,
}
```

`FileContent::bytes(bytes)` produces `inline`; `FileContent::blob(id)` produces `blob`, keeping large bytes in the host's disk-backed cache rather than crossing the WIT.

## Effects: the terminal host-mutation channel

```wit
variant effect {
    project(proj-entry),
    invalidate-path(string),
    invalidate-prefix(string),
    disown-tree(tree-handoff),
}

record proj-entry {
    path: string,
    kind: entry-kind,
    listing-exhaustive: bool,
}
```

`project` installs adjacent/nested projections into the cache (see [Project everything](./project-everything/)); `invalidate-path`/`invalidate-prefix` drop cached entries (see [Cache invalidation](./cache-invalidation/)); `disown-tree` installs a handed-off tree. You produce these through `Projection` (`proj`, `proj_file`, `proj_many`, `proj_dir`) and through the `Effects` returned from `on_event` (`invalidate_path`, `invalidate_prefix`); the `disown-tree` effect is emitted for you when a `#[treeref]` handler returns a `TreeRef`.

## Continuation: resume and cancel

```wit
interface continuation {
    use types.{callout-results, correlation-id, provider-step};

    resume: func(id: correlation-id, results: callout-results) -> provider-step;
    cancel: func(id: correlation-id);
}
```

After a `suspended`, the host runs the callouts and calls `resume(id, results)` with one `callout-result` per callout, in order. The provider's continuation, keyed by `id`, runs to the next suspension or a return. `cancel` drops an in-flight continuation. The SDK's async runtime manages all of this behind `cx.http()`, `cx.git()`, and the other callout builders.

## Callouts and results

```wit
variant callout {
    fetch(http-request),
    fetch-blob(blob-fetch-request),
    open-archive(archive-open-request),
    git-open-repo(git-open-request),
    read-blob(read-blob-request),
    // stream-* and ws-* arms reserved
}

variant callout-result {
    http-response(http-response),
    blob-fetched(blob-fetched),
    archive-opened(archive-opened),
    git-repo-opened(git-repo-info),
    blob-read(list<u8>),
    callout-error(callout-error),
}
```

These back the `Cx` builders: `fetch` ↔ `cx.http().get(..).send()`, `fetch-blob` ↔ `cx.http().get(..).into_blob().with_cache_key(..).send()`, `open-archive` ↔ `cx.archives().open(..).send()`, `git-open-repo` ↔ `cx.git().open_repo(cache_key, clone_url)`, `read-blob` ↔ `cx.blob(id).read()`. A `callout-error` surfaces in the handler as a `Result::Err`.

## Lifecycle and notify

```wit
interface lifecycle {
    initialize: func(config: list<u8>) -> provider-return;
    shutdown: func();
}

interface notify {
    on-event: func(id: correlation-id, event: provider-event) -> provider-step;
}

variant provider-event {
    file-changed(string),
    webhook-received(list<u8>),
    timer-tick(timer-tick-context),
    auth-refreshed,
}
```

`initialize` receives the config JSON bytes and is **terminal-only** — no correlation id, so it cannot suspend on callouts. `on-event` delivers outside-world changes; its returned effects (invalidations) are applied at the response boundary. The `#[omnifs_sdk::provider]` macro generates a default no-op `on-event` unless your provider overrides it.

:::note
`open-file`, `read-chunk`, `close-file`, and the `stream-*` / `ws-*` callouts are reserved in the WIT for streamed and ranged access. The current host/runtime path serves exact file bytes via `read-file` and explicit subtree handoff. Build against `read-file` and blob/tree handoff unless you are specifically extending the ranged path.
:::
