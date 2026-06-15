---
title: "Error model"
description: "Provider and callout error kinds exposed through the WIT boundary."
---

Shared error kinds:

| Kind | Meaning |
|---|---|
| `not-found` | Requested resource does not exist. |
| `not-a-directory` | Path segment expected a directory. |
| `not-a-file` | Path expected a file. |
| `permission-denied` | Provider or upstream denied permission. |
| `invalid-input` | Path, capture, config, or request input is invalid. |
| `too-large` | Requested payload is too large for this path. |
| `network` | Network or upstream transport failure. |
| `timeout` | Timed out. |
| `denied` | Host capability policy denied the request. |
| `rate-limited` | Upstream or host rate limit. May include `retry-after`. |
| `version-mismatch` | Cached or supplied version no longer matches. |
| `internal` | Provider or host internal failure. |

Use specific errors in provider code. Avoid hiding policy denials, not-found, and rate limits inside generic internal errors.
