# omnifs-mount-schema

Wire types for the `omnifs.provider-manifest.v1` custom section that omnifs WASM provider components embed. The host parses this section to discover provider mount points and capabilities without instantiating the component; the SDK emits it during compilation.

This crate is an implementation detail of the [omnifs](https://github.com/0xff-ai/omnifs) project. Provider authors don't depend on it directly: they use `omnifs-sdk`, which re-exports the relevant types and handles the embedding via the `#[provider]` macro.

## What's in here

`ProviderManifest`, `ProviderCapabilities`, `AuthManifest`, and the manifest enum types that the host runtime reads. `wasmparser`-backed helpers extract omnifs custom sections from `.wasm` component files.

## Install

```toml
[dependencies]
omnifs-mount-schema = "0.1"
```

## Status

Pre-1.0. Wire format may evolve.

## License

Dual licensed under MIT or Apache-2.0 at your option.
