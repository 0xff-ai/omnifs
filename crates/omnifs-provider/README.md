# omnifs-provider

The provider contract for omnifs WASM provider components: the `omnifs.provider-manifest.v1` and `omnifs.provider-metadata.v1` custom sections, route resolution, the provider capability model, config metadata, and the auth-scheme model. The host parses these sections to discover provider mount points and capabilities without instantiating the component; the SDK emits them during compilation.

This crate is an implementation detail of the [omnifs](https://github.com/0xff-ai/omnifs) project. Provider authors don't depend on it directly: they use `omnifs-sdk`, which re-exports the relevant types and handles the embedding via the `#[provider]` macro.

## What's in here

`ProviderManifest`, `ProviderCapabilities`, `AuthManifest`, and the manifest enum types that the host runtime reads. `wasmparser`-backed helpers extract omnifs custom sections from `.wasm` component files.

## Install

```toml
[dependencies]
omnifs-provider = "0.2"
```

## Status

Pre-1.0. Wire format may evolve.

## License

Dual licensed under MIT or Apache-2.0 at your option.
