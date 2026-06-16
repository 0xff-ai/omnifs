---
title: "Provider manifest"
description: "The omnifs.provider.json fields used for provider metadata, defaults, capabilities, auth, and config schema."
---

`omnifs.provider.json` is provider metadata embedded into the provider component.

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

## Fields

| Field | Meaning |
|---|---|
| `id` | Stable provider id. |
| `displayName` | Human-readable name. |
| `provider` | WASM component filename. |
| `defaultMount` | Default mount name used by `omnifs init`. |
| `capabilities` | Declared needs, each with `kind`, `value`, `why`, and optional `dynamic`. |
| `auth` | Auth schemes and header injection rules. |
| `configSchema` | Provider-specific config schema for init prompts and validation. |

The manifest declares needs. The host resolves and grants runtime authority.
