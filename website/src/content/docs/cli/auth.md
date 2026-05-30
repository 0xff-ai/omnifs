---
title: Authentication
description: Manage provider credentials from the CLI — omnifs auth login, logout, import, and status.
---

The `omnifs auth` group manages provider credentials in the host credential
store. omnifs picks one credential backend at startup: the OS keychain, or a file
fallback at `~/.omnifs/data/credentials.json`. Both the `auth` write path and the
`omnifs up` read path point at the same store, so logging in once is enough for
the container to pick up the credential at launch.

Credentials are keyed by `provider:scheme:account` (for example
`github:pat:default`). They live on the host and are materialized into a
per-session directory when `omnifs up` starts the container; they are never baked
into the image.

```bash
omnifs auth <login|logout|import|status> [flags]
```

## `omnifs auth login`

```bash
omnifs auth login <provider> [--device]
```

Logs in to a provider via OAuth. By default it uses a loopback redirect when one
is available and falls back to the device flow otherwise. The resulting
credential is written to the host store. The device flow auto-copies the user
code, shows a countdown, and opens the verification URL for you.

| Flag / argument | Purpose |
|-----------------|---------|
| `<provider>` | Provider to authenticate (e.g. `github`, `linear`). |
| `--device` | Force the device flow even when a loopback redirect is available. |
| `--no-browser` | Print the OAuth URL instead of opening a browser. |

```bash
omnifs auth login github
omnifs auth login github --device
omnifs auth login linear --no-browser
```

## `omnifs auth logout`

```bash
omnifs auth logout <provider>
```

Removes the stored credential for a provider. It derives the credential key for
the provider and deletes the matching entry from the host store.

| Argument | Purpose |
|----------|---------|
| `<provider>` | Provider to log out of (e.g. `github`). |

```bash
omnifs auth logout github
```

## `omnifs auth import`

```bash
omnifs auth import <provider> [flags]
```

Imports an existing token into the host store, for providers that issue personal
access tokens or API keys instead of running an OAuth flow. The token is read
from stdin or a named environment variable — never from a command argument.

| Flag / argument | Purpose |
|-----------------|---------|
| `<provider>` | Provider to import a token for (e.g. `github`). |
| `--token` | Read the token from stdin. |
| `--token-env <VAR>` | Read the token from the named environment variable. |
| `--scheme <SCHEME>` | Credential scheme (e.g. `pat`, `bearer`). Provider default if omitted. |

```bash
echo "$MY_TOKEN" | omnifs auth import github --token
omnifs auth import github --token-env GITHUB_TOKEN --scheme pat
```

:::caution
There is no `--token <value>` form. Use `--token` (stdin) or `--token-env <VAR>`
so secrets never land in shell history.
:::

## `omnifs auth status`

```bash
omnifs auth status [--json]
```

Shows credential status per configured mount. It enumerates mounts, resolves each
mount's credential key, and reports whether a credential is present, its source
(host store, `token_env`, or `token_file`), and its scheme.

| Flag | Purpose |
|------|---------|
| `--json` | Emit a machine-readable payload. |

```bash
omnifs auth status
omnifs auth status --json
```

:::note
Mounts can also reference external secrets directly through `token_env` or
`token_file` in their config, instead of the host store. `auth status` reports
those as externally configured.
:::
