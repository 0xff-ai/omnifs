---
title: Authoring guide
description: "Build a provider with the omnifs SDK: provider shape, config, routes, objects, capabilities, and the validation loop."
---

A provider is a `wasm32-wasip2` component that teaches omnifs how one system appears as paths. The host owns the surface, credentials, cache, and callouts. Your provider owns routes, object identity, and rendering.

Provider authoring is usable inside the omnifs workspace and still stabilizing. For exact edge cases, compare against the provider implementations under `providers/`.

## Provider shape

A provider has:

- a config type,
- a state type,
- a `#[omnifs_sdk::provider]` impl,
- route registrations in `start`,
- handlers for files, directories, tree refs, or object leaves,
- an `omnifs.provider.json` manifest.

Workspace providers use one top-level provider impl:

```rust
use omnifs_sdk::prelude::*;

struct DbProvider;

#[omnifs_sdk::provider(metadata = "omnifs.provider.json")]
impl DbProvider {
    type Config = Config;
    type State = State;

    fn start(config: Config, r: &mut Router<State>) -> Result<State> {
        let state = State::open(config)?;

        r.dir("/tables").handler(tables_list)?;
        r.file("/tables/{table}/schema.sql").handler(table_schema_sql)?;

        Ok(state)
    }
}
```

There is no `#[dir]` or `#[file]` route attribute. Routes are registered imperatively.

## Config

Use `#[omnifs_sdk::config]` for the Rust config type. The macro wires JSON deserialization for provider startup. The public schema is still authored in `omnifs.provider.json`.

```rust
#[omnifs_sdk::config]
struct Config {
    path: String,
    sample_limit: Option<u32>,
}
```

Do not rely on Rust config structs to generate the manifest schema. Keep the manifest explicit.

## Routes

| Method | Use |
|---|---|
| `r.dir(path).handler(handler)` | Directory listing and lookup behavior. |
| `r.file(path).handler(handler)` | File bytes. |
| `r.treeref(path).handler(handler)` | Hand off a real backing tree, such as a cloned repo. |
| `r.object::<O>(path, \|o\| { ... })` | Directory-shaped object with representations and leaves. |
| `r.file_object::<O>(path, \|o\| { ... })` | File-shaped object. |
| `r.attach(path, &subtree)` | Attach a detached object subtree at a prefix. |

Variable segments use `{name}` captures. Capture field types parse with `FromStr`; a parse failure removes that route from candidacy.

## Objects

Object-shaped providers declare canonical resources once and render leaves from them. A fresh load stores canonical upstream bytes and materialized view leaves. A warm read renders from host-pushed canonical bytes without an upstream call.

```rust
r.object::<TableDoc>("/tables/{table}", |o| {
    o.representations("table", ())?;
    o.file("schema.sql").handler(table_schema_sql)?;
    o.file("schema.json").handler(table_schema_json)?;
    o.file("count.txt").handler(table_count_txt)?;
    o.file("sample.json").handler(table_sample)?;
    Ok(())
})?;
```

The object type needs a declaration, a typed key, and a `Key::load` implementation. That load function returns `Fresh`, `Unchanged`, or `NotFound` and supplies canonical bytes for the object cache.

Use object routes when several files describe the same upstream resource. Use plain file routes for independent leaves.

## Macro resources and manifest

The provider macro can declare compile-time resources used by typed endpoints and git handoffs:

```rust
#[omnifs_sdk::provider(
    metadata = "omnifs.provider.json",
    resources(endpoints = [api::GitHubApi], git = true)
)]
impl GitHubProvider {
    /* ... */
}
```

The manifest declares package metadata, default mount name, capabilities, auth, and config schema. See [Config, manifests, and capabilities](./config-manifests-and-capabilities.md) for the full manifest reference.

## Validation loop

During SDK work, validate the provider as code and as a filesystem surface:

```bash
just providers-check
cargo check -p omnifs-provider-db --target wasm32-wasip2
cargo test -p omnifs-provider-db --target wasm32-wasip2 --no-run
omnifs dev -y
omnifs shell
```

Inside the shell, test with `ls`, `cat`, `jq`, `find`, and `grep`. A provider is not done when the handler compiles. It is done when normal tools can traverse the path surface without special knowledge.
