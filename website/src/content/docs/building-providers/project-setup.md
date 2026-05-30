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

[package.metadata.docs.rs]
default-target = "wasm32-wasip2"

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

That brings in the contexts (`Cx`, `DirCx`, `BindCtx`), the projection types (`Projection`, `PageStatus`, `Cursor`, `FileContent`, `FileStat`, `TreeRef`), `FileProj`/`FileAttrs`/`Size`/`Stability`/`ReadMode`, `Effects`, `Result`/`ProviderError`, the attribute macros (`provider`, `handlers`, `config`, `subtree`, `dir`, `file`, `treeref`, `bind`, `mutate`), and the curated WIT types (`ProviderInfo`, `RequestedCapabilities`, `ProviderEvent`, …). You do **not** call `wit_bindgen::generate!` yourself; the SDK does it once and re-exports the bindings.

## State, config, and the entrypoint

A provider has a `State` type (your runtime data) and an optional `#[omnifs_sdk::config]` type (parsed from the mount JSON). The entrypoint struct carries `#[provider(...)]`, which points at the embedded manifest, lists the handler modules, and provides an `init` function. `init` returns `(State, ProviderInfo, RequestedCapabilities)`, or `Result<(State, ProviderInfo, RequestedCapabilities)>` when it can fail.

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

#[provider(
    metadata = "omnifs.provider.json",
    mounts(crate::root::RootHandlers)
)]
impl HelloProvider {
    fn init(config: Config) -> (State, ProviderInfo, RequestedCapabilities) {
        let greeting = config.greeting.unwrap_or_else(|| "hi".into());
        (
            State { greeting },
            ProviderInfo {
                name: "hello-provider".into(),
                version: "0.1.0".into(),
                description: "Minimal example provider".into(),
            },
            RequestedCapabilities::empty(),
        )
    }
}
```

`mounts(...)` lists the **handler structs** (qualified by module) that carry `#[handlers]` blocks. The macro stitches their route tables together and implements the WIT `provider` world (lifecycle, browse, continuation, notify). `RequestedCapabilities::empty()` requests nothing extra; `::with_git(refresh_secs)` requests git plus a poll interval.

## A handler module that compiles

```rust
// root.rs
use omnifs_sdk::prelude::*;
use crate::State;

pub struct RootHandlers;

#[handlers]
impl RootHandlers {
    #[dir("/")]
    fn root(_cx: &DirCx<State>) -> Result<Projection> {
        let mut p = Projection::new();
        p.deferred_file("hello.txt");
        p.page(PageStatus::Exhaustive);
        Ok(p)
    }

    #[file("/hello.txt")]
    fn hello(cx: &Cx<State>) -> Result<FileContent> {
        let greeting = cx.state(|s| s.greeting.clone());
        Ok(FileContent::bytes(format!("{greeting}\n")))
    }
}
```

The handler struct (`RootHandlers`) is the grouping the entrypoint references; routes are registered by path pattern. The state type is inferred from the `Cx<State>` / `DirCx<State>` in your handler signatures. Handlers can be `fn` or `async fn`; the context parameter is optional when a handler needs neither config nor callouts.

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

This manifest is embedded into the WASM and read by the host and CLI. See [Auth manifest](./auth-manifest/) for the `auth`/`capabilities` blocks and [Config](./config/) for `configSchema`.

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
