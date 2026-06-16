---
title: "WIT interface"
description: "The current omnifs provider WIT surface: provider steps, callouts, effects, lifecycle, namespace, continuation, and notify."
---

Current package:

```text
omnifs:provider@0.4.0
```

## Provider step

```text
suspended(list<callout>)
returned(provider-return)
```

Returned steps carry an operation result and effects. They do not carry trailing callouts.

## Provider return

```wit
record provider-return {
    %result: op-result,
    effects: effects,
}
```

The operation result answers the call. The effects record is the only terminal host-mutation channel.

## Operations

| Interface | Functions |
|---|---|
| Lifecycle | `initialize`, `shutdown` |
| Namespace | `lookup-child`, `list-children`, `read-file`, `open-file`, `read-chunk`, `close-file` |
| Continuation | `resume`, `cancel` |
| Notify | `on-event` |

`initialize` is lifecycle setup. Browse operations live under `namespace`. Suspended operations resume through `continuation.resume`. Provider events enter through `notify.on-event`.

## Callouts

- `fetch`
- `git-open-repo`
- `fetch-blob`
- `open-archive`
- `read-blob`

Callouts are request/response. A provider cannot emit fire-and-forget host work.

## Effects

- `canonical`
- `fs`
- `invalidations`

```wit
record effects {
    canonical: list<canonical-store>,
    fs: list<fs-write>,
    invalidations: list<invalidation>,
}
```

`canonical` stores raw upstream bytes under a logical object id. `fs` writes files or directories into the view cache. `invalidations` evict cached objects or listings.

## File metadata

The WIT file metadata shape uses:

- `file-size`: `exact(u64)`, `non-zero`, or `unknown`,
- `byte-source`: `inline`, `canonical`, `blob`, or `deferred`,
- `read-mode`: `full` or `ranged`,
- `stability`: `immutable`, `mutable`, or `volatile`.

See [File attributes](file-attributes.md) for the host policy derived from those declarations.

For exact record fields, use `crates/omnifs-wit/wit/provider.wit` in the implementation repo.
