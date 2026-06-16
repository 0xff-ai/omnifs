---
title: "Environment variables"
description: "Environment variables and overrides used by the omnifs CLI and runtime setup paths."
---

Common override variables:

| Variable | Use |
|---|---|
| `OMNIFS_PROVIDERS_DIR` | Override provider WASM directory. |
| `OMNIFS_MOUNTS_DIR` | Override mount config directory. |
| `OMNIFS_CONTAINER_NAME` | Override runtime container name. |
| `RUST_LOG` | Override CLI/runtime tracing filter. |
| `GITHUB_TOKEN` | Common static-token source for GitHub import/dev flows. |
| `LINEAR_TOKEN` | Common static-token source for Linear dev flows. |

Prefer explicit CLI flags when documenting reproducible commands. Use environment variables when a task needs secret import or local override behavior.
