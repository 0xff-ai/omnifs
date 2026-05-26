# omnifs-sdk

SDK for building [omnifs](https://github.com/0xff-ai/omnifs) providers. Providers are `wasm32-wasip2` components that the omnifs host loads and drives through a WIT interface; this crate turns a Rust impl block into a complete provider component with the right manifest, dispatch wiring, and runtime glue.

## Quick start

```rust
use omnifs_sdk::*;

#[config]
pub struct Config {
    pub greeting: String,
}

pub struct MyProvider { cfg: Config }

#[handlers]
impl MyProvider {
    #[file("/hello.txt")]
    fn hello(&self, _: Path) -> Result<FileContent> {
        Ok(FileContent::text(format!("{}, world\n", self.cfg.greeting)))
    }

    #[dir("/items")]
    fn list_items(&self, _: Path) -> Result<Projection> {
        Ok(Projection::new()
            .file_with_content("a.txt", b"first\n")
            .file_with_content("b.txt", b"second\n"))
    }
}

#[provider(mounts("hello"))]
impl MyProvider {
    fn new(cfg: Config) -> Result<Self> {
        Ok(Self { cfg })
    }
}
```

Build with `cargo build --target wasm32-wasip2 --release` and the resulting `.wasm` component is ready for `omnifs mount`.

## Concepts

- **Path-first handlers**: handler signatures take a parsed `Path` and return either a terminal result or a list of `callout`s for the host to execute (HTTP fetch, git open). The host calls `resume(id, results)` to continue.
- **Auto-navigable directories**: any literal-segment prefix of a registered route is implicitly a directory; no stub handlers needed.
- **Typed subtrees**: a `#[subtree] impl B { ... }` block can be mounted at any `#[bind("/path/{capture}/...")]` site for clean handoff.
- **Capabilities**: providers declare HTTP domains, auth types, memory limits, and git/websocket flags in their manifest; the host enforces them.

The [path-dispatch-and-listing design doc](https://github.com/0xff-ai/omnifs/blob/main/docs/design/path-dispatch-and-listing.md) is the source of truth for routing precedence and listing semantics.

## Install

```toml
[dependencies]
omnifs-sdk = "0.1"
```

Add `crate-type = ["cdylib", "lib"]` and target `wasm32-wasip2`. The provided `omnifs-cli` host loads the resulting `.wasm` component.

## Status

Pre-1.0. Provider authoring API may evolve; minor versions track breaking SDK changes for now.

## License

Dual licensed under MIT or Apache-2.0 at your option.
