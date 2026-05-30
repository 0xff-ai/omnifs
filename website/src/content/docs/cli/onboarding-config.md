---
title: Onboarding and configuration
description: Get omnifs configured — guided setup, single-provider init, mount management, and a full reset.
---

These verbs create and manage the configuration that `omnifs up` consumes: mount
config files under `~/.omnifs/config/mounts/` and the credentials they reference.

## `omnifs setup`

```bash
omnifs setup [flags]
```

The guided onboarding walkthrough — sequential and npx-style, with each step
printed inline. It is a thin orchestrator over `init` and `up`, not a parallel
implementation. In one narrative flow it:

1. detects your host OS and explains the alpha runtime model,
2. verifies Docker and eagerly pulls the image,
3. lists already-configured providers and presents a picker of the rest,
4. confirms capabilities and runs `init` per selected provider,
5. launches the container (unless `--no-up`).

`setup` is re-runnable. Already-configured providers are listed but excluded from
the picker, so you can add more later without redoing prior work. It requires a
TTY.

| Flag | Purpose |
|------|---------|
| `--no-up` | Write configs but skip the final container launch. |
| `-y`, `--yes` | Skip confirmations; auto-accept detected ambient credentials. |
| `--providers <LIST>` | Comma-separated providers to preselect, skipping the picker. |
| `--no-browser` | Print the OAuth URL instead of opening a browser. |

```bash
omnifs setup
# ... pick providers, authenticate ...
# Launching the omnifs container…

omnifs setup --providers github,dns -y
```

## `omnifs init`

```bash
omnifs init [<provider>] [flags]
```

Interactive setup for a single mount. It resolves the provider's manifest,
chooses a mount name, runs the appropriate auth flow (OAuth, token import, or
no-auth), and writes the mount config to
`~/.omnifs/config/mounts/<name>.json`. A picker is shown if the provider is
omitted. Built-in providers include `github`, `linear`, `dns`, and `db` (SQLite).

| Flag / argument | Purpose |
|-----------------|---------|
| `<provider>` | Provider to configure (e.g. `github`, `linear`). Picker shown if omitted. |
| `--as <NAME>` | Mount name override. Auto-generated from the provider if absent. |
| `--token <-> ` | Read the static token from stdin (pass `-`). |
| `--token-env <VAR>` | Read the static token from the named environment variable. |
| `--scope <SCOPE>` | OAuth scope to request. Repeat for multiple scopes. |
| `--yes` | Accept the auto-suggested mount name on collision (never overwrite). |
| `--no-input` | Skip prompts. Static-token providers also require `--token` or `--token-env`. |
| `--no-browser` | Print the OAuth URL instead of opening a browser. |

```bash
omnifs init github                      # interactive, OAuth by default
omnifs init github --as work-gh         # custom mount name
omnifs init linear --token-env LINEAR_API_KEY
echo "$TOKEN" | omnifs init linear --token -
```

:::caution
Tokens are accepted only via `--token -` (stdin) or `--token-env <VAR>`. There is
no `--token <value>` form, to keep secrets out of shell history.
:::

:::note
Before starting OAuth, `init` probes for existing credentials — provider env
vars and, for GitHub, `gh auth status`. The default is to start OAuth; using a
detected source requires explicit confirmation.
:::

## `omnifs mounts`

```bash
omnifs mounts rm <name> [flags]
```

Manages configured mounts. Its one subcommand is `rm`, which removes a mount and,
by default, the stored credential it references. Listing is covered by
[`omnifs status`](/cli/lifecycle/) and selection by `init`, so only removal needs
its own verb.

### `omnifs mounts rm`

```bash
omnifs mounts rm <name> [flags]
```

Removes a configured mount. The name is validated before any path is constructed,
so it cannot escape the mounts directory. By default it also deletes the mount's
stored credential, in the same flow (the credential key is derived from the
config it is about to delete).

| Flag / argument | Purpose |
|-----------------|---------|
| `<name>` | Mount name to remove. |
| `--force` | Skip the confirmation prompt. |
| `--keep-credentials` | Keep the stored credential; remove only the config. |

```bash
omnifs mounts rm work-gh
omnifs mounts rm work-gh --keep-credentials --force
```

:::note
Credentials sourced externally (`token_env`, `token_file`) are reported as
unchanged — only host-store credentials are deleted.
:::

## `omnifs reset`

```bash
omnifs reset [flags]
```

The clean-slate verb. It removes the container first (so nothing is mid-mount),
then deletes every mount config and, by default, every stored credential. It
asks for confirmation unless `--yes` is set.

| Flag | Purpose |
|------|---------|
| `-y`, `--yes` | Skip the confirmation prompt. |
| `--keep-credentials` | Keep stored credentials; only remove configs and the container. |
| `--container-name <NAME>` | Container name. Defaults to `OMNIFS_CONTAINER_NAME`, then `omnifs`. |

```bash
omnifs reset
# This removes all mount configs and stored credentials and the `omnifs` container.
# Continue? (y/N)

omnifs reset --keep-credentials --yes
```

:::caution
`omnifs reset` is destructive. Without `--keep-credentials` it deletes stored
credentials as well as mount configs. Run `omnifs setup` afterward to start
fresh.
:::
