---
title: Config, manifests, and capabilities
description: Provider config types, the omnifs.provider.json manifest schema, and how to declare auth schemes and runtime capability grants.
---

A provider has Rust config and a manifest. They serve different purposes: the Rust config type is what the provider receives at startup; the manifest is the public contract embedded in the component.

## Rust config

Use `#[omnifs_sdk::config]` for the config type the provider receives at initialization:

```rust
#[omnifs_sdk::config]
struct Config {
    endpoint: String,
    sample_limit: Option<u32>,
}
```

The macro wires JSON deserialization for provider startup and sets `deny_unknown_fields`, so mount JSON typos fail initialization loudly. Do not rely on the Rust config struct to generate the manifest schema. Keep the manifest explicit.

## Manifest

`omnifs.provider.json` is provider metadata embedded into the component. The provider macro reads it at compile time via the `metadata` argument.

```json
{
  "id": "github",
  "displayName": "GitHub",
  "provider": "omnifs_provider_github.wasm",
  "defaultMount": "github",
  "capabilities": [],
  "auth": {},
  "configSchema": {}
}
```

### Manifest fields

| Field | Meaning |
|---|---|
| `id` | Stable provider id used as the credential storage key prefix. |
| `displayName` | Human-readable name shown in `omnifs status`. |
| `provider` | WASM component filename. |
| `defaultMount` | Default mount name suggested by `omnifs init`. |
| `capabilities` | Declared needs, each with `kind`, `value`, `why`, and optional `dynamic`. |
| `auth` | Auth schemes and header injection rules. |
| `configSchema` | Provider-specific config schema for init prompts and validation. |

The manifest declares needs. The host resolves and grants runtime authority.

## Mount config

Mount specs are JSON. A mount selects a provider, a mount name, optional auth, optional config, and optional runtime capability overrides. The host resolves raw mount config into runtime-ready mounts by applying provider metadata and defaults.

## Capabilities

Provider authors declare what they need. The host decides what is granted.

Use narrow capabilities:

- Exact domains for HTTP callouts.
- Explicit WASI preopens only when the provider needs a local file capability.
- Git repo patterns only when tree handoff is required.
- Blob fetch/read byte limits when the provider works with large host-managed bytes.
- Auth injection only for domains that need credentials.

A full capabilities example:

```json
{
  "capabilities": [
    {
      "kind": "domain",
      "value": "api.example.com",
      "why": "Fetch Example API resources."
    },
    {
      "kind": "memoryMb",
      "value": 64,
      "why": "Declare the provider's expected memory need."
    }
  ]
}
```

Do not treat a `memoryMb` declaration as proof that long-lived provider memory is enforced. Document it as requested or declared memory unless runtime enforcement changes.

## Auth schemes

Add auth only when the provider needs credentials. Auth schemes describe how the host obtains credentials and which domains it may inject them into. The `auth` block in the manifest controls this.

Providers must not parse tokens out of config or logs. The host credential store is not accessible to provider WASM.
