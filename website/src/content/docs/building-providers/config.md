---
title: Provider Config
description: Declaring config with #[omnifs_sdk::config], the JSON config object, and reading it from a handler.
---

Each mount carries a JSON config object. The host parses the mount JSON, preserves the provider-specific `config` object as raw JSON, and passes its bytes to your provider's `initialize()`. Your provider deserializes those bytes into a typed struct. Configs are JSON, never TOML.

## Declaring a config struct

Annotate the struct with `#[omnifs_sdk::config]`. The macro derives `serde::Deserialize` and wires the struct into `initialize()` config parsing. Deriving `Default` is recommended so a missing or empty config object still produces a usable value.

```rust
use omnifs_sdk::prelude::*;

#[omnifs_sdk::config]
#[derive(Default)]
struct DbConfig {
    /// Map of database name -> file path inside the container.
    databases: std::collections::BTreeMap<String, String>,
}
```

Field types are anything `serde` can deserialize. Use `Option<T>` for optional fields and apply your own defaults at use site:

```rust
#[omnifs_sdk::config]
#[derive(Default)]
struct ArxivConfig {
    /// Max results per query (default applied in the handler).
    max_results: Option<u32>,
}
```

## Reading config in a handler

Call `cx.config::<T>()` from any handler that takes `cx: &Cx`. It deserializes the config bytes the host supplied at initialize time and returns your struct (or a `ProviderError` if the JSON does not match).

```rust
#[dir("{category}")]
fn category_dir(category: &str, cx: &Cx) -> Result<List> {
    let cfg = cx.config::<ArxivConfig>()?;
    let max = cfg.max_results.unwrap_or(50);
    let feed = query_arxiv(cx, &format!("cat:{category}"), max)?;
    Ok(List::entries(Listing::partial(feed.entries.iter().map(|e| Entry::dir(arxiv_id(&e.id))))))
}
```

## The matching mount JSON

The mount JSON for an instance places provider settings under a `config` object. The `databases` field above maps directly to it:

```json
{
  "mount": "db",
  "provider": "db",
  "auth": { "scheme": "none" },
  "config": {
    "databases": {
      "chinook": "/run/fixtures/test.db"
    }
  }
}
```

The host extracts the `config` object, serializes it back to JSON bytes, and hands those bytes to `initialize()`. Your `DbConfig` deserializes from exactly that object — top-level mount keys like `mount`, `provider`, and `auth` are **not** part of your config struct.

## What does and does not belong in config

- Provider behavior knobs: API base URL overrides, page sizes, resolver addresses, database path maps. These belong in `config`.
- External secret references for static mounts may use `token_env` or `token_file` at the mount/auth level — not inside your provider config struct, and never as a keychain indirection. Host-managed credentials are derived from the auth manifest, not from your config. See [Auth manifest](./auth-manifest/).

:::tip
Keep the config struct small and declarative. A handler should be able to read everything it needs with a single `cx.config::<T>()?` call near the top.
:::

:::caution
`initialize()` is terminal-only: it cannot suspend on callouts. Do not try to perform network I/O during config parsing. Do that lazily inside browse handlers, where suspend/resume is available.
:::
