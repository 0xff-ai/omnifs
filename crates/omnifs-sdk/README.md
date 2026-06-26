# omnifs-sdk

SDK for building [omnifs](https://github.com/0xff-ai/omnifs) providers. Providers are `wasm32-wasip2` components; the host drives them through the `omnifs:provider` WIT interface. This crate supplies routing, projections, callouts, and the `#[provider]` macro that wires WIT exports to your `Router`.

## Quick start

```rust
use omnifs_sdk::prelude::*;

#[provider(id = "my-provider")]
impl MyProvider {
    fn start(r: &mut Router) -> Result<()> {
        r.file("/hello.txt").handler(hello)?;
        r.dir("/items").handler(list_items)?;
        Ok(())
    }
}

async fn hello(_cx: Cx) -> Result<FileProjection> {
    Ok(FileProjection::body(b"hello, world\n").build())
}

async fn list_items(_cx: DirCx) -> Result<DirProjection> {
    Ok(DirProjection::exhaustive([
        Entry::file("a.txt"),
        Entry::file("b.txt"),
    ]))
}
```

Build with `cargo build --target wasm32-wasip2 --release`. The `.wasm` component is what `omnifs` mounts.

## Concepts

- **Router registration**: `Router::dir`, `Router::file`, `Router::treeref`, `Router::object::<Object>()`, and reusable object handles register path families at `start`. Literal path prefixes are auto-navigable directories; you do not write stub `dir` handlers for intermediate segments.
- **Provider lifecycle**: the `#[provider]` macro infers config and state from `start`. Use `fn start(r: &mut Router) -> Result<()>` for stateless providers, or `fn start(config: Config, r: &mut Router<State>) -> Result<State>` when config or state exists.
- **Handlers**: async functions taking `Cx`, `DirCx`, state-bearing `Cx<State>` / `DirCx<State>`, or typed `#[path_captures]` keys. Return `FileProjection`, `DirProjection`, `TreeRef`, or `Effects` (for timer/event handlers).
- **Objects**: `#[object]` types implement `Object::load` / `Object::render`; `bind` mounts them at a path template. The host caches canonical bytes and pushes them on later reads.
- **Endpoints**: `#[derive(Endpoint)]` plus `cx.endpoint::<E>()` for typed HTTP (and rate-limit policy) against declared bases.
- **Callouts**: handlers `.await` on `cx.http()`, `cx.git()`, etc. The host executes the batch and calls `resume`; there are no fire-and-forget callouts.
- **Projections**: `FileProjection` / `DirProjection` encode size, stability, byte source, and additional file, directory, and canonical effects that should be materialized with the accepted result. Listings use `FileProj::listing_shape()` for file entries named before content is fetched.

See the [provider SDK contract](https://github.com/0xff-ai/omnifs/blob/main/docs/contracts/20-provider-sdk.md) and [projection tree contract](https://github.com/0xff-ai/omnifs/blob/main/docs/contracts/30-projection-tree.md) for routing precedence, pagination (`@next` / `@all`), and attribute rules.

## Install

```toml
[dependencies]
omnifs-sdk = "0.1"
```

Add `crate-type = ["cdylib", "lib"]` and target `wasm32-wasip2`.

## Status

Pre-1.0. The v2 authoring surface (`Router`, projections, objects) is the supported path; legacy `#[handlers]` / `#[subtree]` attributes were removed.

## License

Dual licensed under MIT or Apache-2.0 at your option.
