---
title: Project Setup
description: Create a provider crate — Cargo.toml, the provider entrypoint, mount modules, and the wasm32-wasip2 build.
---

This page gets you from an empty directory to a provider that compiles to a `wasm32-wasip2` component.

## The crate

Providers live under `providers/<name>/` as workspace members named `omnifs-provider-<name>`. The package-name glob `omnifs-provider-*` is how the workspace selects providers for builds and checks, so keep the prefix.

```toml
# providers/hello/Cargo.toml
[package]
name = "omnifs-provider-hello"
version.workspace = true
edition.workspace = true
description = "omnifs example provider"

[lib]
crate-type = ["cdylib", "lib"]

[dependencies]
omnifs-sdk = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
```

`crate-type = ["cdylib", "lib"]` is required: the `wasm32-wasip2` target emits a component from the cdylib, and the `lib` form lets host-target unit tests link the crate. Depend on `omnifs-sdk`; it re-exports `serde`, `serde_json`, and `hashbrown` so generated code can reference them.

:::tip
Use `hashbrown::HashMap` (re-exported as `omnifs_sdk::hashbrown`) for provider-internal maps. It keeps behavior predictable across WASI targets.
:::

## The single import

Everything you need comes from the prelude:

```rust
use omnifs_sdk::prelude::*;
```

That brings in `Cx`, `Init`, `Entry`, `Listing`, `FileContent`, `TreeRef`, `Effects`, `Projection`, `Lookup` types, `FileProj`, `Size`, `Stability`, `ReadMode`, the `ResponseExt` HTTP trait, `Result`/`ProviderError`, the attribute macros (`provider`, `handlers`, `config`, `subtree`, `dir`, `file`, `treeref`, `bind`, `mutate`), and the curated WIT types (`ProviderEvent`, `OpResult`, and friends). You do **not** call `wit_bindgen::generate!` yourself; the SDK does it once and re-exports the bindings.

## State, config, and the entrypoint

A provider has a `State` type (your runtime data) and an optional `#[omnifs_sdk::config]` type (parsed from the mount JSON). The entrypoint struct carries `#[omnifs_sdk::provider(...)]`, which names the state type, the config type, and the handler modules, and provides an `init` function returning `Init<State>`.

```rust
// lib.rs
use omnifs_sdk::prelude::*;

mod provider;
mod root;

#[omnifs_sdk::config]
#[derive(Default)]
pub(crate) struct Config {
    greeting: Option<String>,
}

#[derive(Clone)]
pub(crate) struct State {
    greeting: String,
}
```

```rust
// provider.rs
use omnifs_sdk::prelude::*;
use crate::{Config, State};

struct HelloProvider;

#[omnifs_sdk::provider(state = State, config = Config, mounts(crate::root))]
impl HelloProvider {
    fn init(config: Config) -> Result<Init<State>> {
        let greeting = config.greeting.unwrap_or_else(|| "hi".into());
        Ok(Init::new(State { greeting }))
    }
}
```

`mounts(...)` lists the **modules** that contain `#[handlers]` blocks. The macro stitches their route tables together and implements the WIT `provider` world (lifecycle, browse, continuation, notify). `Init::new(state)` requests default capabilities; chain `.with_info(name, version, description)` to override the manifest-derived provider info.

## A handler module that compiles

```rust
// root.rs
use omnifs_sdk::prelude::*;
use crate::State;

#[omnifs_sdk::handlers(state = State)]
impl Root {
    #[dir("/")]
    async fn root(_cx: Cx<State>) -> Result<Listing> {
        Ok(Listing::complete(vec![Entry::file(
            "hello.txt",
            FileProj::deferred(Size::NonZero, ReadMode::Full, Stability::Immutable),
        )]))
    }

    #[file("/hello.txt")]
    async fn hello(cx: Cx<State>) -> Result<FileContent> {
        let greeting = cx.state(|s| s.greeting.clone());
        Ok(FileContent::new(format!("{greeting}\n")))
    }
}
```

The `impl Root` block name is just a grouping; the macro registers each handler by its path pattern, not by the impl name. Every handler is `async fn` and takes `cx: Cx<State>` as its first parameter.

## The manifest

Pair the crate with `omnifs.provider.json` at the crate root:

```json
{
  "id": "hello",
  "displayName": "Hello",
  "provider": "omnifs_provider_hello.wasm",
  "defaultMount": "hello",
  "capabilities": [
    { "kind": "memoryMb", "value": 32, "why": "Tiny example provider." }
  ]
}
```

This manifest is embedded into the WASM and read by the host and CLI. See [Auth manifest](./auth-manifest/) for the `auth` block and [Config](./config/) for `configSchema`.

## Building

Build the component directly with the Rust target — there is no separate componentization step:

```bash
cargo build -p omnifs-provider-hello --target wasm32-wasip2
```

Add `--release` to emit `target/wasm32-wasip2/release/omnifs_provider_hello.wasm`. To build or check every provider at once:

```bash
just providers-build   # release-build all omnifs-provider-* and omnifs-tool-*
just providers-check   # wasm32-wasip2 check + clippy for the same set
```

:::caution
Provider clippy and test commands must include `--target wasm32-wasip2`. Host-native checks will not catch WASI-specific compilation problems. See [Testing](./testing/) for the full command set.
:::
