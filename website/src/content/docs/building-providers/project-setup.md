---
title: Project Setup
description: Create a provider crate — Cargo.toml, WIT bindings, mount declaration, and the wasm32-wasip2 build.
---

This page gets you from an empty directory to a provider that compiles to a `wasm32-wasip2` component.

## The crate

Providers live under `providers/<name>/` as workspace members named `omnifs-provider-<name>`. The package-name glob `omnifs-provider-*` is how the workspace selects providers for builds and checks, so keep the prefix.

```toml
# providers/hello/Cargo.toml
[package]
name = "omnifs-provider-hello"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
omnifs-sdk = { path = "../../crates/omnifs-sdk" }
serde.workspace = true
serde_json.workspace = true

[package.metadata.component]
package = "omnifs:provider-hello"
```

The `crate-type = ["cdylib"]` line is required: the `wasm32-wasip2` target emits a component from a cdylib. Depend only on `omnifs-sdk`; it re-exports `serde`, `serde_json`, and `hashbrown` so generated code can reference them without you adding direct dependencies.

:::tip
Use `hashbrown::HashMap` (re-exported as `omnifs_sdk::hashbrown`) for provider-internal maps. It keeps behavior predictable across WASI targets.
:::

## WIT bindings

You do **not** call `wit_bindgen::generate!` in your own crate. The SDK generates the bindings once, in `omnifs-sdk/src/lib.rs`, and re-exports everything you need:

```rust
// crates/omnifs-sdk/src/lib.rs (already done for you)
wit_bindgen::generate!({
    world: "provider",
    path: "../../wit",
    pub_export_macro: true,
});
```

In your provider, just import the prelude:

```rust
use omnifs_sdk::prelude::*;
```

That brings in `Cx`, `Entry`, `Listing`, `List`, `Lookup`, `FileContent`, `Effects`, `FileProj`, `Size`, `Stability`, `ReadMode`, the `Request`/`Response` HTTP helpers, `Result`/`ProviderError`, and the handler attribute macros.

## Declaring mounts

Every provider has a struct entrypoint annotated with `#[omnifs_sdk::provider(mounts(...))]`. The `mounts(...)` list names the mount points this provider serves; each name must match a `mount` entry in `omnifs.provider.json`.

```rust
struct HelloProvider;

#[omnifs_sdk::provider(mounts(hello))]
impl HelloProvider {}
```

The macro wires up the WIT `provider` world exports (lifecycle, browse, continuation, notify) and assembles a route table from every `#[omnifs_sdk::handlers]` and `#[omnifs_sdk::subtree]` block in the crate. The impl body is usually empty; your logic lives in the handlers.

## A skeleton that compiles

```rust
use omnifs_sdk::prelude::*;

#[omnifs_sdk::config]
#[derive(Default)]
struct HelloConfig {
    greeting: Option<String>,
}

struct HelloProvider;

#[omnifs_sdk::provider(mounts(hello))]
impl HelloProvider {}

#[omnifs_sdk::handlers]
impl HelloProvider {
    #[dir("")]
    fn root() -> Result<List> {
        Ok(List::entries(Listing::complete([Entry::file(
            "hello.txt",
            FileProj::inline(b"hi\n".to_vec(), Stability::Immutable, None),
        )])))
    }

    #[file("hello.txt")]
    fn hello(cx: &Cx) -> Result<FileContent> {
        let greeting = cx.config::<HelloConfig>()?.greeting.unwrap_or_else(|| "hi".into());
        Ok(FileContent::new(format!("{greeting}\n")))
    }
}
```

Pair it with a manifest:

```json
{
  "schema": "omnifs.provider/v1",
  "id": "hello",
  "name": "Hello",
  "description": "Minimal example provider.",
  "mounts": [{ "mount": "hello", "description": "A greeting." }],
  "auth": { "scheme": "none" }
}
```

## Building

Build the component directly with the Rust target — no separate componentization step:

```bash
cargo build -p omnifs-provider-hello --target wasm32-wasip2
```

The release artifact is emitted to `target/wasm32-wasip2/release/omnifs_provider_hello.wasm` when you add `--release`. To build every provider at once:

```bash
just providers-build
```

To check (without producing artifacts), use the provider check recipe, which compiles for the WASI target and runs clippy:

```bash
just providers-check
```

:::caution
Provider clippy and test commands must include `--target wasm32-wasip2`. Host-native checks will not catch WASI-specific compilation problems. See [Testing](./testing/) for the exact command set.
:::
