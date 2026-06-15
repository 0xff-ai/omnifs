---
title: "Capability types"
description: "Reference for provider capability declarations and runtime security checks."
---

| Kind | Value shape | Notes |
|---|---|---|
| `domain` | Hostname string or explicit `*` wildcard | Built-in manifests should use exact hosts; the runtime checker also accepts `*` as allow-all. |
| `gitRepo` | Repo pattern | Used by Git tree handoff. |
| `unixSocket` | Socket path or dynamic configured endpoint | Exact-path allowlisted. |
| `preopenedPath` | `{ host, guest, mode }` | Explicit WASI preopen. |
| `memoryMb` | Number | Declared memory need, not documented as enforced long-lived limit. |
| `fetchBlobBytes` | Number | Byte ceiling declaration for host-managed blob fetches. |
| `readBlobBytes` | Number | Byte ceiling declaration for host-managed blob reads. |

Auth injection is described by manifest `auth.inject`, not as a generic provider-held secret.
