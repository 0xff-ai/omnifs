---
title: Authentication
description: Manage provider credentials from the CLI — omnifs auth login, logout, import, and status.
---

The `omnifs auth` group manages mount credentials in the host credential store.
omnifs picks one credential backend at startup: the OS keychain, or a file
fallback at `~/.omnifs/data/credentials.json`. Both the `auth` write path and the
`omnifs up` read path point at the same store, so authenticating once is enough
for the container to pick up the credential at launch.

The auth subcommands operate on a **mount name** (the name you gave a mount with
[`omnifs init`](/cli/onboarding-config/)), defaulting to the provider's default
mount when omitted. Credentials are keyed by `provider:scheme:account` (for
example `github:pat:default`). They live on the host and are materialized into a
per-session directory when `omnifs up` starts the container; they are never baked
into the image.

```bash
omnifs auth <login|logout|import|status> [flags]
```

## `omnifs auth login`

```bash
omnifs auth login [<mount>] [flags]
```

Logs in to a configured mount through its provider's OAuth flow and writes the
resulting credential to the host store. The mount defaults to the provider's
default mount if omitted.

| Flag / argument | Purpose |
|-----------------|---------|
| `<mount>` | Mount name to authenticate. Defaults to the provider's default mount. |
| `--no-browser` | Print the OAuth URL instead of opening a browser. |
| `--scope <SCOPE>` | OAuth scope to request. Repeat for multiple scopes. |

```bash
omnifs auth login github
omnifs auth login github --no-browser
omnifs auth login github --scope repo --scope read:org
```

## `omnifs auth logout`

```bash
omnifs auth logout [<mount>]
```

Removes the stored credential for a mount. It resolves the mount's credential
target and deletes the matching entry from the host store.

| Argument | Purpose |
|----------|---------|
| `<mount>` | Mount name whose stored credential should be removed. Defaults to the provider's default mount. |

```bash
omnifs auth logout github
```

## `omnifs auth import`

```bash
omnifs auth import [<mount>] [flags]
```

Imports an existing token into the host store for a mount, for providers that
issue personal access tokens or API keys instead of running an OAuth flow. The
token is read from stdin or a named environment variable — never from a command
argument.

| Flag / argument | Purpose |
|-----------------|---------|
| `<mount>` | Mount name to import a token for. Defaults to the provider's default mount. |
| `--token <-> ` | Read the token from stdin (pass `-`). |
| `--token-env <VAR>` | Read the token from the named environment variable. |
| `--scheme <KEY>` | Scheme key to import under. Defaults to the provider's static scheme. |

```bash
echo "$MY_TOKEN" | omnifs auth import github --token -
omnifs auth import github --token-env GITHUB_TOKEN --scheme pat
```

:::caution
Tokens are accepted only via `--token -` (stdin) or `--token-env <VAR>`. There is
no `--token <value>` form, so secrets never land in shell history.
:::

## `omnifs auth status`

```bash
omnifs auth status [--json]
```

Shows credential status per configured mount. It enumerates mounts, resolves each
mount's credential, and reports whether one is present, its source (host store,
`token_env`, or `token_file`), and its scheme.

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
