# omnifs-sdk-macros

Proc macros for the [omnifs](https://github.com/0xff-ai/omnifs) provider SDK. They define provider entrypoints, object metadata, typed route captures, config metadata, and HTTP endpoints.

This crate is re-exported through `omnifs-sdk`. Provider authors should depend on `omnifs-sdk` and import macros from there; depending on `omnifs-sdk-macros` directly is not supported.

## Macros

- `#[provider(...)]` defines one provider implementation. Its synchronous `start` method registers routes on a `Router`; the macro wires lifecycle, async namespace, notify exports, and static provider metadata.
- `#[object(...)]` defines an object's identity and canonical format while the object builder owns stability and rendering.
- `#[path_captures]` turns a named-field struct into a typed multi-segment route key.
- `#[path_segment(...)]` generates parsing and rendering for one route segment.
- `#[config]` derives the provider config dialect used in metadata and initialization.
- `#[derive(Endpoint)]` declares a typed outbound HTTP endpoint and its rate-limit policy.

See the [provider SDK contract](https://github.com/0xff-ai/omnifs/blob/main/docs/contracts/20-provider-sdk.md) for the routing precedence and listing semantics behind these macros.

## Install

```toml
[dependencies]
omnifs-sdk = "0.2"
# omnifs-sdk-macros is re-exported through omnifs-sdk; do not depend on it directly.
```

## Status

Pre-1.0. Macro surface evolves alongside the SDK.

## License

Dual licensed under MIT or Apache-2.0 at your option.
