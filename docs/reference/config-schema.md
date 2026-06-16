---
title: "Config schema"
description: "The mount config shape omnifs reads and the provider config schema embedded in provider manifests."
---

Mount specs are JSON.

```json
{
  "provider": "github",
  "mount": "github",
  "auth": {
    "type": "oauth",
    "scheme": "device"
  },
  "config": {}
}
```

The host loads raw mount specs, resolves them against provider metadata, applies defaults, and produces runtime-ready mounts.

## Top-level fields

| Field | Required | Meaning |
|---|---:|---|
| `provider` | yes | Provider id, such as `github` or `db`. |
| `mount` | yes | Mount name under `/omnifs`. |
| `auth` | no | Selected auth scheme or external token source. |
| `config` | no | Provider-specific JSON config. |
| `capabilities` | no | Runtime grant overrides or additions. |

Provider-specific config schemas live in `omnifs.provider.json`. Docker requires `endpoint`. db requires `database_type` and `path`.

## Source of truth

Use the provider manifest and mount-schema implementation as the source of truth. Do not infer schema from examples alone.
