# omnifs-sdk-macros

Proc macros for the [omnifs](https://github.com/raulk/omnifs) provider SDK. These power the path-first handler model that providers use to project external services into FUSE-visible paths.

This crate is re-exported through `omnifs-sdk`. Provider authors should depend on `omnifs-sdk` and import macros from there; depending on `omnifs-sdk-macros` directly is not supported.

## Macros

- `#[handlers] impl ...` — collects free-function path handlers from an impl block.
- `#[dir]`, `#[file]`, `#[treeref]`, `#[bind]`, `#[mutate]` — annotate handlers inside a `#[handlers]` block by their FUSE op kind.
- `#[subtree] impl B { ... }` — typed subtree dispatch, mountable at any number of `#[bind("...")]` sites in the parent.
- `#[config]` — provider config struct, deserialized from JSON by the host.
- `#[provider(mounts(...))]` — provider entrypoint; declares mount points and wires the manifest custom section.

See the [SDK design notes](https://github.com/raulk/omnifs/blob/main/docs/design/path-dispatch-and-listing.md) for the routing precedence and listing semantics behind these macros.

## Install

```toml
[dependencies]
omnifs-sdk = "0.1"
# omnifs-sdk-macros is re-exported through omnifs-sdk; do not depend on it directly.
```

## Status

Pre-1.0. Macro surface evolves alongside the SDK.

## License

Dual licensed under MIT or Apache-2.0 at your option.
