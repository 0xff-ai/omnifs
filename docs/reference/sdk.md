---
title: "SDK API"
description: "The author-facing omnifs SDK concepts: provider macro, config macro, router, object model, projections, endpoints, and errors."
---

This is a concept-organized reference for the current SDK surface. It is not generated rustdoc.

## Provider entrypoint

```rust
#[omnifs_sdk::provider(metadata = "omnifs.provider.json")]
impl Provider {
    type Config = Config;
    type State = State;

    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        Ok(State::new(config)?)
    }
}
```

## Config

Use `#[omnifs_sdk::config]` for provider startup config deserialization.

## Router

- `r.dir`
- `r.file`
- `r.treeref`
- `r.object`
- `r.file_object`
- `r.attach`

There are no per-route attribute macros.

## Objects

Use `#[omnifs_sdk::object]`, an object key, and `Key::load` to define canonical object identity and rendered leaves.

`Key::load` returns fresh, unchanged, or not found state. Fresh loads can provide canonical bytes and validators.

## Projections

Directory and file handlers return projected entries and file content with attributes, content type, and byte source.

## Errors

Use provider errors that map cleanly to the WIT error model: not found, invalid input, permission denied, denied, rate limited, network, timeout, too large, version mismatch, and internal.
