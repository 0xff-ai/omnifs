---
title: Auth and credential custody
description: How omnifs stores credentials, resolves mount auth, injects approved headers into callouts, and keeps providers away from the credential store.
---

Credentials belong to the host.

Providers declare auth schemes in their manifests. Mounts select a scheme. The host stores the credential, refreshes it when needed, and injects approved headers into approved HTTP callouts. Providers receive response bytes, not the credential store.

## Supported patterns

| Pattern | Use |
|---|---|
| OAuth | GitHub device flow and Linear OAuth are host-managed flows. |
| Static token | Personal access tokens or API keys imported into the host credential store. |
| No auth | Some providers do not use API tokens. Check the provider page and manifest for the exact requirement. |

GitHub supports a device flow and a PAT scheme. Linear supports OAuth and a PAT/API-key scheme.

## Credential storage

The host stores credentials in a file-backed store at `~/.omnifs/credentials.json`, with private directories and private file permissions on Unix. This is the production backend by design, not a fallback: a known path with enumerable keys and atomic writes works the same in containers and headless environments, where a desktop secret service is not available.

At runtime, credentials needed by the container session are materialized into a private per-session directory. That materialization is host work. Providers still do not receive the full credential store.

## Header injection

Auth injection is scoped by provider manifest. This table is an example drawn from built-in manifests, not a maintained exhaustive list. Use the provider manifests as the source of truth.

| Provider | Injection domains | Header |
|---|---|---|
| GitHub | `api.github.com` | `Authorization: Bearer ...` |
| Linear | `api.linear.app` | `Authorization: ...` |

The host injects credentials only for configured domains. A provider cannot take a stored GitHub token and send it to an arbitrary host through a normal fetch callout.

## Commands

```bash
omnifs auth status
omnifs auth login github
omnifs auth refresh github
omnifs auth import github --token-env GITHUB_TOKEN
omnifs auth logout github
```

Use `--no-browser` for login flows that should print the URL instead of opening a browser.

## Operational guidance

- Prefer least-privilege tokens.
- Keep static-token mounts scoped to the provider and account that need them.
- Re-run `omnifs auth status` after changing mounts.
- Do not share raw traces or logs as proof that credentials are absent.
