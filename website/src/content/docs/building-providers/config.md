---
title: Provider Config
description: Declaring config with #[omnifs_sdk::config], the JSON config object, init, and reading config through provider state.
---

Each mount carries a JSON config object. The host parses the mount JSON and passes the provider-specific config to your provider's `init` function, which deserializes it into a typed struct and folds the result into provider `State`. Configs are JSON, never TOML.

## Declaring a config struct

Annotate the struct with `#[omnifs_sdk::config]`. The macro wires up serde deserialization through the SDK's re-exported serde. Deriving `Default` is recommended so a missing or empty config object still produces a usable value; use `#[serde(default = "...")]` for per-field defaults.

```rust
use omnifs_sdk::prelude::*;
use std::collections::BTreeMap;

#[omnifs_sdk::config]
struct Config {
    #[serde(default = "default_resolver_name")]
    default_resolver: String,
    #[serde(default)]
    resolvers: BTreeMap<String, ConfigResolver>,
}

fn default_resolver_name() -> String { "cloudflare".into() }

#[omnifs_sdk::config]
struct ConfigResolver {
    url: String,
    #[serde(default)]
    aliases: Vec<String>,
}
```

Field types are anything serde can deserialize. When a field is an enum, derive `Deserialize` with the SDK's re-exported serde so it works on the WASI target:

```rust
use omnifs_sdk::serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(crate = "omnifs_sdk::serde", rename_all = "lowercase")]
pub(crate) enum DatabaseType { Sqlite }
```

## Consuming config in init

The `#[provider(...)]` entrypoint's `init` function takes your config type by value — the macro deserializes the mount's config object into it. `init` builds the provider `State` and returns it together with `ProviderInfo` and `RequestedCapabilities`:

```rust
#[provider(metadata = "omnifs.provider.json", mounts(crate::tables::TableHandlers))]
impl DbProvider {
    fn init(config: Config) -> Result<(State, ProviderInfo, RequestedCapabilities)> {
        let backend = SqliteBackend::open(&config.path, config.read_only)
            .map_err(|e| ProviderError::internal(format!("open db: {e}")))?;
        Ok((
            State { config, backend: Rc::new(RefCell::new(backend)) },
            ProviderInfo { name: "db-provider".into(), version: "0.1.0".into(), description: "Relational DB provider".into() },
            RequestedCapabilities::empty(),
        ))
    }
}
```

Store whatever the handlers need on `State`. Handlers then read it through `cx.state(|s| ...)`:

```rust
#[file("/sample.json")]
fn sample(cx: &BindCtx<'_, State, TableSubtree>) -> Result<FileContent> {
    let limit = cx.state(|s| s.config.sample_limit);
    // ...
}
```

`init` is synchronous and cannot perform callouts (see the caution below). Do lazy, network-dependent work inside browse handlers, not in `init`.

## The matching mount JSON

The mount JSON places provider settings under a `config` object. A `db` provider config maps directly:

```json
{
  "mount": "db",
  "provider": "db",
  "config": {
    "database_type": "sqlite",
    "path": "/data/test.db",
    "read_only": true,
    "sample_limit": 20
  }
}
```

Top-level mount keys like `mount`, `provider`, and `auth` are **not** part of your config struct; only the `config` object is deserialized into it.

## Declaring a config schema

You can describe the config shape for the CLI/host in `omnifs.provider.json` via `configSchema` (JSON Schema). This drives `omnifs init` prompts and validation; the `x-omnifs-init` extension can mark a field as a host file to preopen into the sandbox:

```json
"configSchema": {
  "type": "object",
  "required": ["database_type", "path"],
  "properties": {
    "path": {
      "type": "string",
      "default": "/data/test.db",
      "x-omnifs-init": { "input": "host-file", "guestDir": "/data", "preopenMode": "ro" }
    },
    "sample_limit": { "type": "integer", "minimum": 1, "default": 20 }
  }
}
```

## What does and does not belong in config

- Provider behavior knobs: API base URL overrides, page sizes, resolver addresses, database paths and limits. These belong in `config`.
- Secrets do not. Host-managed credentials are derived from the auth manifest and injected at the callout boundary — never read a token from config. See [Auth manifest](./auth-manifest/).

:::caution
`init` (and the underlying `initialize()`) is terminal-only: it has no correlation id and cannot suspend on callouts. Do not perform network I/O in `init`. Defer it to browse handlers, where suspend/resume is available.
:::
